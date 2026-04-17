//! `CargoTestImpact` — Sprint 5 v2 default for Rust workspaces.
//!
//! Runs `cargo test --no-run` once at construction (the `discover`
//! entry point) to compile tests, then `cargo test -- --list` to
//! enumerate the discoverable test universe as `pkg::path::name`
//! identifiers. `select` then ranks the universe by two heuristics:
//!
//! 1. **Direct filename match** (confidence 1.0) — every changed
//!    file's stem (e.g. `foo.rs` → `foo`) is searched for as a
//!    substring inside every test id. Catches `tests/foo_*.rs` and
//!    in-crate `mod tests { fn foo_... }` modules.
//! 2. **Co-edit adjacency** (confidence 0.6) — when a
//!    `GraphRetrieval` (the Sprint 3 `CoEditGraphRetrieval`) is
//!    wired in, immediate co-edit neighbours of each changed file
//!    are resolved to their stems, and tests matching those stems
//!    are appended with a lower confidence score.
//!
//! Symbol-graph caller chasing (plan §Sprint 5 heuristic 3) is
//! deferred to v2.1 — it requires a `references_of` affordance the
//! current `SymbolRetrieval` trait does not expose, and adding that
//! surface is out of Sprint 5 scope.
//!
//! ## Why the pure-select path is extracted
//!
//! Shelling out to `cargo` inside a `cargo test` run is a recipe
//! for flaky CI (nested locks, recompile storms). The pure
//! `select_impacted_tests` function takes a pre-materialised
//! `TestUniverse` so the integration test can feed a synthetic one
//! without ever invoking cargo recursively. The live `discover`
//! path is only exercised from headless dogfood runs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::process::Command;

use azoth_core::impact::{ImpactError, ImpactSelector};
use azoth_core::retrieval::{GraphRetrieval, NodeRef};
use azoth_core::schemas::{Contract, Diff, TestId, TestPlan};

/// Selector-impl version. Bump whenever the ranking heuristic
/// changes so `SessionEvent::ImpactComputed.selector_version`
/// replays correctly reflect plan drift.
pub const CARGO_TEST_IMPACT_VERSION: u32 = 1;

/// The discoverable test set for a cargo workspace. Produced by
/// [`discover_cargo_tests`]; accepts hand-seeded test ids in
/// integration tests via [`TestUniverse::from_tests`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TestUniverse {
    pub tests: Vec<TestId>,
}

impl TestUniverse {
    pub fn from_tests<I, T>(tests: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<TestId>,
    {
        Self {
            tests: tests.into_iter().map(Into::into).collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.tests.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tests.is_empty()
    }
}

/// Sprint 5 default selector. Construction is explicit (no
/// `Default`) because every consumer must commit to either
/// `with_universe` (tests) or `discover` (production). A missing
/// universe would silently short-circuit every plan to empty.
pub struct CargoTestImpact {
    repo_root: PathBuf,
    universe: TestUniverse,
    co_edit: Option<Arc<dyn GraphRetrieval>>,
}

impl CargoTestImpact {
    /// Construct with an already-materialised universe. The
    /// integration test uses this to feed a synthetic universe and
    /// skip the `cargo test --list` shell-out entirely.
    pub fn with_universe(repo_root: PathBuf, universe: TestUniverse) -> Self {
        Self {
            repo_root,
            universe,
            co_edit: None,
        }
    }

    /// Production entry point: shell out to cargo to discover the
    /// universe, then return a ready-to-call selector.
    pub async fn discover(repo_root: PathBuf) -> Result<Self, ImpactError> {
        let universe = discover_cargo_tests(&repo_root).await?;
        Ok(Self::with_universe(repo_root, universe))
    }

    /// Wire the Sprint 3 co-edit graph. Safe to omit — the
    /// selector falls back to the direct-filename heuristic only.
    pub fn with_co_edit_graph(mut self, graph: Arc<dyn GraphRetrieval>) -> Self {
        self.co_edit = Some(graph);
        self
    }

    pub fn universe(&self) -> &TestUniverse {
        &self.universe
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }
}

#[async_trait]
impl ImpactSelector for CargoTestImpact {
    fn name(&self) -> &'static str {
        "cargo_test"
    }

    fn version(&self) -> u32 {
        CARGO_TEST_IMPACT_VERSION
    }

    async fn select(&self, diff: &Diff, _contract: &Contract) -> Result<TestPlan, ImpactError> {
        if self.universe.is_empty() || diff.is_empty() {
            return Ok(TestPlan::empty(self.version()));
        }

        // Widen the diff to include co-edit neighbours, then drive
        // the pure selector so tests can exercise the same code
        // path with hand-seeded universe + graph.
        let mut widened: Vec<String> = diff.changed_files.clone();
        let mut rationale: Vec<(String, String)> = diff
            .changed_files
            .iter()
            .map(|p| (p.clone(), format!("changed file {p}")))
            .collect();

        if let Some(graph) = self.co_edit.as_ref() {
            for changed in &diff.changed_files {
                let neighbors = graph
                    .neighbors(path_node_ref(changed), 1, 20)
                    .await
                    .map_err(|e| ImpactError::Backend(Box::new(e)))?;
                for (node, _edge) in neighbors {
                    if let Some(path) = node.0.strip_prefix("path:") {
                        if !widened.iter().any(|p| p == path) {
                            widened.push(path.to_string());
                            rationale.push((
                                path.to_string(),
                                format!("co-edit neighbour of {changed}"),
                            ));
                        }
                    }
                }
            }
        }

        Ok(select_impacted_tests(
            &self.universe,
            &widened,
            &rationale,
            self.version(),
        ))
    }
}

/// Pure, IO-free selection kernel. Extracted so the integration
/// test can feed a synthetic universe without ever shelling out to
/// cargo. Callers pass the already-widened list of paths plus a
/// per-path rationale so direct-vs-neighbour provenance survives
/// into `TestPlan.rationale`.
///
/// `widened_paths[i]` and `rationale[i]` must correspond. Invariant
/// is guarded by `debug_assert!` at the call seam.
pub fn select_impacted_tests(
    universe: &TestUniverse,
    widened_paths: &[String],
    rationale: &[(String, String)],
    selector_version: u32,
) -> TestPlan {
    // Short-circuit on empty universe — there is nothing to match
    // against. Guards debug_assert! against test callers that pass
    // `&[]` rationale alongside a populated path list.
    if universe.is_empty() {
        return TestPlan::empty(selector_version);
    }

    debug_assert_eq!(
        widened_paths.len(),
        rationale.len(),
        "widened_paths and rationale must align by index"
    );

    let mut plan = TestPlan::empty(selector_version);
    let mut seen: HashSet<String> = HashSet::new();

    for (idx, path) in widened_paths.iter().enumerate() {
        let stem = file_stem(path);
        if stem.is_empty() {
            continue;
        }
        for t in &universe.tests {
            if !t.0.contains(&stem) {
                continue;
            }
            if !seen.insert(t.0.clone()) {
                continue;
            }
            let (why, confidence) = match rationale.get(idx) {
                Some((_, reason)) if reason.starts_with("co-edit") => (reason.clone(), 0.6_f32),
                Some((_, reason)) => (reason.clone(), 1.0_f32),
                None => (format!("match on stem {stem}"), 0.8_f32),
            };
            plan.tests.push(t.clone());
            plan.rationale.push(why);
            plan.confidence.push(confidence);
        }
    }

    debug_assert!(plan.is_well_formed());
    plan
}

/// Shell out: `cargo test --no-run -q` to compile + `cargo test
/// -- --list` to enumerate. Parses the `mod::path::name: test`
/// plain-text format emitted by libtest on stable — `--format=json`
/// is nightly-only, not an acceptable v2 dep.
///
/// Returns `ImpactError::CargoMetadata` (compile phase) or
/// `ImpactError::TestDiscovery` (list phase) on non-zero cargo exit
/// so the `ImpactValidator` can report the failure instead of
/// silently producing an empty plan (an empty plan looks the same
/// as "no impact" on the wire — we want the failure to be
/// distinguishable).
///
/// PR #9 gemini MED: cargo stderr is captured (truncated to 4KB) and
/// folded into the error detail so a failure on a large workspace
/// produces a diagnosable message instead of a bare exit code.
pub async fn discover_cargo_tests(repo_root: &Path) -> Result<TestUniverse, ImpactError> {
    let compile = Command::new("cargo")
        .arg("test")
        .arg("--workspace")
        .arg("--no-run")
        .arg("-q")
        .current_dir(repo_root)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| ImpactError::CargoMetadata(format!("cargo --no-run spawn: {e}")))?;
    if !compile.status.success() {
        return Err(ImpactError::CargoMetadata(format!(
            "cargo test --no-run failed ({status}): {stderr}",
            status = compile.status,
            stderr = truncate_stderr(&compile.stderr)
        )));
    }

    let list = Command::new("cargo")
        .arg("test")
        .arg("--workspace")
        .arg("-q")
        .arg("--")
        .arg("--list")
        .current_dir(repo_root)
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| ImpactError::TestDiscovery(format!("cargo --list spawn: {e}")))?;
    if !list.status.success() {
        return Err(ImpactError::TestDiscovery(format!(
            "cargo test -- --list failed ({status}): {stderr}",
            status = list.status,
            stderr = truncate_stderr(&list.stderr)
        )));
    }

    let text = String::from_utf8_lossy(&list.stdout);
    Ok(parse_cargo_list(&text))
}

/// Render captured stderr bytes for inclusion in an `ImpactError`
/// detail string. UTF-8 is lossy-decoded so a binary accident never
/// crashes the error path; output is capped at `MAX_STDERR_BYTES`
/// with an explicit truncation marker so users can tell when the
/// upstream message is longer than what they see.
const MAX_STDERR_BYTES: usize = 4096;

fn truncate_stderr(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.len() <= MAX_STDERR_BYTES {
        trimmed.to_string()
    } else {
        let cutoff = trimmed
            .char_indices()
            .take_while(|(i, _)| *i < MAX_STDERR_BYTES)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        format!(
            "{head}…[truncated {more} bytes]",
            head = &trimmed[..cutoff],
            more = trimmed.len() - cutoff
        )
    }
}

/// Pure parser for `cargo test -- --list` plain-text output.
/// Separated so tests can feed canned strings.
pub fn parse_cargo_list(text: &str) -> TestUniverse {
    let mut tests = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(name) = trimmed.strip_suffix(": test") {
            let name = name.trim();
            if !name.is_empty() {
                tests.push(TestId::new(name));
            }
        }
    }
    TestUniverse { tests }
}

fn path_node_ref(rel: &str) -> NodeRef {
    NodeRef(format!("path:{rel}"))
}

fn file_stem(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use azoth_core::retrieval::{Edge, GraphRetrieval, RetrievalError};

    fn contract_stub() -> Contract {
        use azoth_core::schemas::{ContractId, EffectBudget, Scope};
        Contract {
            id: ContractId::new(),
            goal: "tdad".into(),
            non_goals: Vec::new(),
            success_criteria: Vec::new(),
            scope: Scope::default(),
            effect_budget: EffectBudget::default(),
            notes: Vec::new(),
        }
    }

    #[test]
    fn parse_cargo_list_extracts_test_names_and_skips_headers() {
        let out = "\nrunning 0 tests\n\
                   azoth_core::foo::tests::bar: test\n\
                   azoth_repo::impact::cargo::tests::quux: test\n\
                   something else: bench\n\
                   garbage line\n";
        let u = parse_cargo_list(out);
        assert_eq!(u.len(), 2);
        assert_eq!(u.tests[0].as_str(), "azoth_core::foo::tests::bar");
        assert_eq!(
            u.tests[1].as_str(),
            "azoth_repo::impact::cargo::tests::quux"
        );
    }

    #[test]
    fn parse_cargo_list_is_whitespace_tolerant() {
        let out = "   azoth::a::b: test   \n";
        let u = parse_cargo_list(out);
        assert_eq!(u.len(), 1);
        assert_eq!(u.tests[0].as_str(), "azoth::a::b");
    }

    #[test]
    fn truncate_stderr_folds_small_output_intact() {
        let out = "error[E0425]: cannot find value `x`\n   --> src/lib.rs:1:1\n";
        let rendered = truncate_stderr(out.as_bytes());
        assert!(rendered.contains("error[E0425]"), "{rendered}");
        assert!(!rendered.contains("[truncated"), "{rendered}");
    }

    #[test]
    fn truncate_stderr_caps_massive_output_with_marker() {
        let big = "x".repeat(MAX_STDERR_BYTES + 500);
        let rendered = truncate_stderr(big.as_bytes());
        assert!(rendered.contains("[truncated"), "{rendered}");
        assert!(rendered.len() <= MAX_STDERR_BYTES + 64); // 64 = marker slack
    }

    #[test]
    fn truncate_stderr_handles_invalid_utf8_without_panicking() {
        // PR #9 gemini MED: lossy-decode so binary stderr (e.g. a
        // linker that dumped raw bytes) never crashes the error
        // path — the reason this sits at a system boundary.
        let bytes = vec![0xff, 0xfe, 0xfd, b'\n', b'o', b'k'];
        let rendered = truncate_stderr(&bytes);
        assert!(rendered.contains("ok"), "{rendered}");
    }

    #[test]
    fn select_impacted_tests_direct_filename_match_hits() {
        let u =
            TestUniverse::from_tests(["my_crate::foo::tests::alpha", "my_crate::bar::tests::beta"]);
        let paths = vec!["src/foo.rs".to_string()];
        let rationale = vec![("src/foo.rs".to_string(), "changed file src/foo.rs".into())];
        let plan = select_impacted_tests(&u, &paths, &rationale, 1);
        assert_eq!(plan.tests.len(), 1);
        assert_eq!(plan.tests[0].as_str(), "my_crate::foo::tests::alpha");
        assert!((plan.confidence[0] - 1.0).abs() < f32::EPSILON);
        assert!(plan.rationale[0].contains("changed file"));
    }

    #[test]
    fn select_impacted_tests_dedupes_when_stem_matches_twice() {
        let u = TestUniverse::from_tests(["my_crate::foo::tests::alpha"]);
        let paths = vec!["src/foo.rs".to_string(), "src/foo/mod.rs".to_string()];
        let rationale = vec![
            ("src/foo.rs".to_string(), "changed file src/foo.rs".into()),
            (
                "src/foo/mod.rs".to_string(),
                "changed file src/foo/mod.rs".into(),
            ),
        ];
        let plan = select_impacted_tests(&u, &paths, &rationale, 1);
        assert_eq!(plan.tests.len(), 1, "dedupe by test id, not by path");
    }

    #[test]
    fn select_impacted_tests_empty_universe_is_empty_plan() {
        let u = TestUniverse::default();
        let plan = select_impacted_tests(&u, &["src/foo.rs".into()], &[], 7);
        assert!(plan.is_empty());
        assert_eq!(plan.selector_version, 7);
    }

    struct MockGraph {
        entries: Vec<(String, Vec<(String, f32)>)>,
    }

    #[async_trait]
    impl GraphRetrieval for MockGraph {
        async fn neighbors(
            &self,
            node: NodeRef,
            _depth: usize,
            _limit: usize,
        ) -> Result<Vec<(NodeRef, Edge)>, RetrievalError> {
            let key = node.0.strip_prefix("path:").unwrap_or(&node.0);
            let hit = self
                .entries
                .iter()
                .find(|(p, _)| p == key)
                .cloned()
                .unwrap_or_default();
            Ok(hit
                .1
                .into_iter()
                .map(|(p, w)| {
                    (
                        NodeRef(format!("path:{p}")),
                        Edge {
                            kind: "co_edit".into(),
                            weight: w,
                        },
                    )
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn selector_folds_in_co_edit_neighbours_with_lower_confidence() {
        let u = TestUniverse::from_tests([
            "my_crate::foo::tests::direct",
            "my_crate::bar::tests::neighbour",
        ]);
        let graph = Arc::new(MockGraph {
            entries: vec![("src/foo.rs".to_string(), vec![("src/bar.rs".into(), 3.5)])],
        });
        let sel =
            CargoTestImpact::with_universe(PathBuf::from("/tmp"), u).with_co_edit_graph(graph);
        let diff = Diff::from_paths(["src/foo.rs"]);
        let plan = sel.select(&diff, &contract_stub()).await.unwrap();
        assert_eq!(plan.tests.len(), 2);
        // Direct match lands first, neighbour second.
        assert!(plan.rationale[0].contains("changed file"));
        assert!(plan.rationale[1].contains("co-edit neighbour"));
        assert!((plan.confidence[0] - 1.0).abs() < f32::EPSILON);
        assert!((plan.confidence[1] - 0.6).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn empty_diff_produces_empty_plan_even_with_universe() {
        let u = TestUniverse::from_tests(["my_crate::foo::tests::alpha"]);
        let sel = CargoTestImpact::with_universe(PathBuf::from("/tmp"), u);
        let plan = sel.select(&Diff::empty(), &contract_stub()).await.unwrap();
        assert!(plan.is_empty());
    }
}
