//! `GoTestImpact` — v2.1 Go ecosystem selector.
//!
//! Shape mirrors [`super::jest::JestImpact`] and [`super::pytest::PytestImpact`]
//! so the TurnDriver swaps selectors by language without reshape:
//!
//! - `with_universe(repo_root, universe)` — synthetic universe for
//!   integration tests, skips the `go test -list` shell-out.
//! - `discover(repo_root)` — production entry point; detects `go.mod`,
//!   rejects `go.work` multi-module repos as
//!   `GoTestError::UnsupportedConfig`, then shells out to
//!   `go test -json -list .*` to enumerate the test universe.
//! - `detect(&Path)` — extension-free detector that returns
//!   `Ok(Some("go_mod"))` on single-module repos,
//!   `Err(UnsupportedConfig)` on `go.work` shapes, `Ok(None)` when no
//!   Go module is present.
//!
//! ## Selection heuristic — package granularity, not file
//!
//! pytest/jest/cargo selectors stem-match against test FILE paths
//! because those ecosystems put test-file names in the test id
//! (`/repo/tests/test_foo.py::test_bar`, `/repo/__tests__/foo.test.ts`).
//! Go's unit of testing is the **package**: `go test ./pkg` compiles
//! `foo.go` + `foo_test.go` together and the emitted test id is
//! `pkg-import-path::TestName` with no file information.
//!
//! So the Go heuristic matches the CHANGED FILE'S PARENT DIRECTORY
//! NAME against each test id's package-path component. Changes to
//! `pkg/auth/tokens.go` pull every test in `example.com/m/pkg/auth`
//! (because `word_boundary_contains("example.com/m/pkg/auth", "auth")`
//! matches). This aligns with Go's compilation model — any change to
//! any file in a package invalidates cached results for that whole
//! package anyway.
//!
//! Confidence is `1.0` for direct package-path matches. Symbol-graph
//! and co-edit widening are deferred to v2.2 (identical rationale to
//! PR-E/F).
//!
//! ## Why `go test -json` for discovery and runner
//!
//! pytest R3-R8 on PR #24 spent 6 parser rewrites on `pytest -v`
//! human-readable output. Go ships a stable NDJSON reporter via
//! `-json` (test2json contract), and `serde_json` is already a
//! workspace dep. Using it from day one avoids the text-parser trap.
//! See `feedback_parser_rewrite_count_is_a_signal.md` in auto-memory.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use thiserror::Error;
use tokio::process::Command;

use azoth_core::impact::{ImpactError, ImpactSelector};
use azoth_core::schemas::{Contract, Diff, TestId, TestPlan};

use super::cargo::TestUniverse;
use super::heuristic::word_boundary_contains;
use super::runner::{TestOutcome, TestRunResult, TestRunSummary, TestRunner};

/// Selector-impl version. Bump on heuristic changes so replay can
/// detect plan drift without re-running the selector.
pub const GOTEST_IMPACT_VERSION: u32 = 1;

/// Forensic-detail truncation cap. Local (not shared) so each runner
/// can diverge independently if its output shape demands a different
/// ceiling — mirrors the pytest/jest pattern.
const MAX_DETAIL_BYTES: usize = 4096;

/// Separator used to encode `TestId` as `<package_import_path>::<TestName>`.
/// Matches the pytest/jest shape so the TUI can split uniformly.
const ID_SEP: &str = "::";

/// Typed error surface for the go-test backend. Boxed into
/// `ImpactError::Backend` at the selector boundary.
#[derive(Debug, Error)]
pub enum GoTestError {
    #[error("go module not detected (no go.mod at repo root)")]
    NotDetected,
    #[error(
        "go multi-module workspace (go.work) unsupported in v2.1 — \
         per-module universes needed; revisit in v2.2"
    )]
    UnsupportedConfig,
    #[error("go test discovery failed: {0}")]
    Discovery(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// v2.1 Go selector. Construction is explicit (no `Default`) because
/// every consumer must commit to `with_universe` (tests) or
/// `discover` (production). A missing universe would silently
/// short-circuit every plan to empty.
pub struct GoTestImpact {
    repo_root: PathBuf,
    universe: TestUniverse,
}

impl GoTestImpact {
    /// Construct with an already-materialised universe. Integration
    /// tests feed synthetic universes; production uses `discover`.
    pub fn with_universe(repo_root: PathBuf, universe: TestUniverse) -> Self {
        Self {
            repo_root,
            universe,
        }
    }

    /// Production entry point: detect `go.mod`, shell out to
    /// `go test -json -list .* ./...`, build the universe.
    ///
    /// `extra_args` are forwarded verbatim to the `go test -list`
    /// command between `test` and the internal flags. R4 gemini HIGH
    /// on PR #26: Go projects often gate tests behind build tags
    /// (`//go:build integration`); without a way to pass `-tags
    /// integration` through, those tests stay invisible to the
    /// selector and TDAD misses them entirely. Mirrors the
    /// `GoTestRunner::extra_args` surface so caller code can pass
    /// the same flags to both discovery and execution.
    pub async fn discover(repo_root: PathBuf, extra_args: &[String]) -> Result<Self, ImpactError> {
        match Self::detect(&repo_root) {
            Ok(Some(_)) => {
                let universe = discover_go_tests(&repo_root, extra_args).await?;
                Ok(Self {
                    repo_root,
                    universe,
                })
            }
            Ok(None) => Err(ImpactError::Backend(Box::new(GoTestError::NotDetected))),
            Err(e) => Err(ImpactError::Backend(Box::new(e))),
        }
    }

    /// Extension-free detector.
    ///
    /// - `Ok(Some("go_mod"))` — single-module repo (`go.mod` present).
    /// - `Ok(None)` — no Go module.
    /// - `Err(UnsupportedConfig)` — `go.work` present (Go 1.18+
    ///   multi-module workspace). v2.1 rejects because each module
    ///   has its own universe and the selector heuristic assumes a
    ///   single module tree; multi-module support revisits in v2.2.
    ///
    /// `go.work` wins over `go.mod` when both are present — the
    /// workspace file supersedes module resolution rules per
    /// [the Go tooling reference](https://go.dev/ref/mod#workspaces).
    /// Trusting only `go.mod` in a workspace would give us a
    /// universe that lies about which modules are in scope.
    pub fn detect(repo_root: &Path) -> Result<Option<&'static str>, GoTestError> {
        if repo_root.join("go.work").exists() {
            return Err(GoTestError::UnsupportedConfig);
        }
        if repo_root.join("go.mod").exists() {
            return Ok(Some("go_mod"));
        }
        Ok(None)
    }

    pub fn universe(&self) -> &TestUniverse {
        &self.universe
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }
}

#[async_trait]
impl ImpactSelector for GoTestImpact {
    fn name(&self) -> &'static str {
        "gotest"
    }

    fn version(&self) -> u32 {
        GOTEST_IMPACT_VERSION
    }

    async fn select(&self, diff: &Diff, _contract: &Contract) -> Result<TestPlan, ImpactError> {
        if self.universe.is_empty() || diff.is_empty() {
            return Ok(TestPlan::empty(self.version()));
        }
        let mut plan = TestPlan::empty(self.version());
        // `HashSet<&str>` borrows from `self.universe` — zero-alloc
        // dedupe, same as PR-E/F.
        let mut seen: HashSet<&str> = HashSet::new();
        // Dedupe parent_path → representative changed-file path. R1
        // gemini MED on PR #26: N changed files × M tests is O(N*M)
        // with repeated redundant work when multiple changed files
        // map to the same package directory. Collapsing to
        // unique-parent-paths first drops the outer loop to
        // O(unique_parents × M) without changing the semantic — the
        // dedupe on `seen` already kept only the FIRST changed file's
        // rationale for each matched test, so recording the
        // representative path here is behaviourally identical.
        let mut by_parent: Vec<(&str, &str)> = Vec::new();
        let mut parent_seen: HashSet<&str> = HashSet::new();
        // R3 gemini MED on PR #26: a `.go` file at the repo root
        // (e.g. `main.go` in a flat project) has empty parent_path,
        // so R2 skipped it entirely. That invalidates TDAD for every
        // flat project — the correct test universe subset is "all
        // tests in the module's base package", but we can't identify
        // the base package without parsing go.mod for the module
        // prefix (deferred to v2.2). Conservative fallback: if any
        // changed file is a `.go` source at the repo root, include
        // the entire universe. Non-Go root files (README.md, config,
        // etc.) stay skipped because they can't affect any Go
        // package's test outcome.
        let mut root_go_change: Option<&str> = None;
        for path in &diff.changed_files {
            // Go-native heuristic: RELATIVE parent-dir-path (full,
            // not just the last component) against the test id's
            // package path. R2 gemini HIGH on PR #26: my R1 used
            // only the last component, which over-selected on
            // common dir names — e.g. a change in
            // `pkg/auth/internal/foo.go` pulled tests from every
            // `*/internal` package in the repo because stem
            // `internal` matched them all. Using the full relative
            // parent `pkg/auth/internal` narrows the match to the
            // specific package suffix. Residual imprecision: if
            // two distinct modules happen to share a long suffix
            // (e.g. `other/pkg/auth` and `pkg/auth` both exist),
            // both match. Tight fix requires parsing go.mod for the
            // module prefix — deferred to v2.2.
            //
            // `rsplit_once('/')` over `Path::parent().file_name()`
            // per gemini MED on PR #26: no Path-object allocation,
            // preserves hierarchy, and uses git-diff-native `/`
            // separator (non-Windows paths only, which matches
            // azoth's Linux-only operational envelope).
            let parent_path = path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
            if parent_path.is_empty() {
                // Root-level file. Only .go matters — anything else
                // (README, config, non-Go toolchain files) cannot
                // affect test outcomes, so stays skipped.
                if path.ends_with(".go") && root_go_change.is_none() {
                    root_go_change = Some(path.as_str());
                }
                continue;
            }
            if parent_seen.insert(parent_path) {
                by_parent.push((parent_path, path.as_str()));
            }
        }
        // Apply the root-level .go fallback FIRST, before parent-path
        // matching, so the entire universe gets included deterministically
        // and the regular matching loop below only adds additional
        // provenance for tests that also match a specific package
        // parent. `seen` deduplicates naturally.
        if let Some(root_path) = root_go_change {
            for t in &self.universe.tests {
                let t_str = t.as_str();
                if seen.insert(t_str) {
                    plan.tests.push(t.clone());
                    plan.rationale.push(format!(
                        "root-level Go change {root_path} → conservative \
                         select-all (go.mod module-prefix parsing deferred to v2.2)"
                    ));
                    plan.confidence.push(1.0);
                }
            }
        }
        for (parent_path, path) in by_parent {
            for t in &self.universe.tests {
                let t_str = t.as_str();
                // Package path is everything left of `::`. Missing
                // separator means the universe entry is malformed —
                // skip rather than crash (defensive; discovery emits
                // the separator on every id).
                let pkg_path = t_str.split_once(ID_SEP).map(|(p, _)| p).unwrap_or("");
                if word_boundary_contains(pkg_path, parent_path) && seen.insert(t_str) {
                    plan.tests.push(t.clone());
                    plan.rationale
                        .push(format!("changed file {path} → pkg dir {parent_path}"));
                    plan.confidence.push(1.0);
                }
            }
        }
        debug_assert!(plan.is_well_formed());
        Ok(plan)
    }
}

/// Shell out to `go test -json -list '.*' ./...` inside `repo_root`
/// and parse the emitted NDJSON events. Each `output` event carrying
/// a bare `TestXxx`/`BenchmarkXxx`/`ExampleXxx` line is a listed test
/// in the package named by the event's `Package` field.
///
/// Non-zero exit → `GoTestError::Discovery` with stdout+stderr merged.
pub async fn discover_go_tests(
    repo_root: &Path,
    extra_args: &[String],
) -> Result<TestUniverse, ImpactError> {
    // `extra_args` land BEFORE the internal flags so a caller passing
    // `-tags integration` or `-tags unit,integration` reaches the
    // compiler-level build-tag filter correctly — `go test` accepts
    // build flags and test flags intermixed, but tags especially are
    // resolved during test-binary compilation which happens before
    // `-list` runs. Mirror of `GoTestRunner::run` argv shape
    // (R4 gemini HIGH on PR #26).
    let mut cmd = Command::new("go");
    cmd.arg("test");
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.arg("-json").arg("-list").arg(".*").arg("./...");
    let out = cmd
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| ImpactError::Backend(Box::new(GoTestError::Io(e))))?;
    if !out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        // Cap the error body — a broken monorepo can dump multi-MB
        // of build errors into stderr and the TUI has to render
        // this string (R3 gemini MED on PR #26). Same 4 KiB cap
        // as per-test detail.
        let merged = cap_to_max_bytes(merge_pipes(&stdout, &stderr), MAX_DETAIL_BYTES);
        return Err(ImpactError::Backend(Box::new(GoTestError::Discovery(
            merged,
        ))));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(parse_list_ndjson(&text))
}

/// Concatenate stdout+stderr for forensic rendering. Shared shape with
/// `jest::merge_pipes` and `pytest::merge_pipes` but kept local so the
/// three backends can evolve independently.
fn merge_pipes(stdout: &str, stderr: &str) -> String {
    if stderr.is_empty() {
        stdout.to_string()
    } else if stdout.is_empty() {
        stderr.to_string()
    } else {
        format!("{stdout}\n{stderr}")
    }
}

/// Single event emitted by `go test -json` / `test2json`. Only the
/// fields we consume are deserialized; `Time` + `Elapsed` optional
/// fields for per-test timing.
#[derive(Debug, Deserialize)]
struct GoTestEvent {
    #[serde(rename = "Action")]
    action: String,
    #[serde(rename = "Package")]
    package: Option<String>,
    #[serde(rename = "Test")]
    test: Option<String>,
    #[serde(rename = "Output")]
    output: Option<String>,
    #[serde(rename = "Elapsed")]
    elapsed: Option<f64>,
}

/// True when `line` matches a Go test function name that is
/// RUNNABLE via `go test -run`. Looser checks like `starts_with("Test")`
/// would accept `"Testing helper: ..."` or `"TestMain with "` (the
/// magic function) which aren't real test listings.
///
/// Rules per `go help testflag` and the testing package source
/// (`testing.matchTests` uses `!unicode.IsLower(first_rune)`):
///
/// - `TestXxx` / `FuzzXxx` — first tail rune must NOT be a lowercase
///   letter. Accepts uppercase ASCII, uppercase Unicode (codex P2 on
///   PR #26 R3: `TestÉclair` is a valid Go test identifier and appears
///   in `go test -list` output), digits, and `_`. Bare `Test` / `Fuzz`
///   are rejected by Go's own test matcher.
/// - `ExampleXxx` OR bare `Example` — the bare form is a package-
///   level runnable example (codex P2 on PR #26 R0). Tail rune, if
///   present, follows the same Unicode non-lowercase rule.
/// - `Benchmark*` — excluded entirely. `go test -run` does NOT execute
///   benchmarks (only `-bench` does per `go help testflag`). Including
///   them in discovery makes the runner report every benchmark as
///   `Unknown` with stdout "no tests to run" (codex P2 on PR #26 R0).
///   Re-visit in v2.2 when a `-bench`-aware runner path is wired.
fn is_test_name_line(line: &str) -> bool {
    for prefix in ["Test", "Example", "Fuzz"] {
        let Some(tail) = line.strip_prefix(prefix) else {
            continue;
        };
        // Bare `Example` is a valid package-level runnable example.
        // Bare `Test` / `Fuzz` are not. Short-circuit on empty tail
        // for Example only.
        if tail.is_empty() {
            return prefix == "Example";
        }
        if let Some(c) = tail.chars().next() {
            // Match Go's `testing.matchTests`: the first rune after
            // the prefix must not be a lowercase letter. This accepts
            // Unicode uppercase (`TestÉclair`), digits (`Test2`), and
            // underscore (`Test_foo`) while still rejecting prose
            // like `Testing` or `Testx`.
            if !c.is_lowercase() {
                return true;
            }
        }
        return false;
    }
    false
}

/// Parse `go test -json -list '.*'` NDJSON output into a
/// [`TestUniverse`]. Output events with a bare `TestXxx` / `BenchmarkXxx`
/// / `ExampleXxx` / `FuzzXxx` line become test ids; everything else
/// (package-level `start`/`pass`, summary `ok PKG` lines, fail-fast
/// diagnostics) is skipped.
///
/// Non-JSON lines are tolerated — the `go` driver occasionally prints
/// to stdout outside the JSON envelope (e.g. build failure messages
/// before test2json activates). Silently ignoring them is correct: if
/// the process exited non-zero, the caller already raised `Discovery`.
fn parse_list_ndjson(text: &str) -> TestUniverse {
    let mut tests: Vec<TestId> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let ev: GoTestEvent = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if ev.action != "output" {
            continue;
        }
        let pkg = match ev.package.as_deref() {
            Some(p) if !p.is_empty() => p,
            _ => continue,
        };
        let raw = match ev.output.as_deref() {
            Some(o) => o,
            None => continue,
        };
        let name = raw.trim();
        if !is_test_name_line(name) {
            continue;
        }
        let id_str = format!("{pkg}{ID_SEP}{name}");
        if seen.insert(id_str.clone()) {
            tests.push(TestId::new(id_str));
        }
    }
    TestUniverse::from_tests(tests)
}

/// Live go-test runner. Guarded behind the `live-tools` feature for
/// its integration test because the `go` toolchain is not a CI
/// dependency.
///
/// Consumes `go test -json` NDJSON output — no text parsing, no
/// per-round edge-case fixes. Package batching: one `go test -json
/// <pkg> -run '^(A|B|...)$' -count=1` invocation per package to
/// avoid the `-run` regex becoming pathologically large when one
/// plan spans many packages.
#[derive(Default)]
pub struct GoTestRunner {
    /// Extra args passed to `go test` BEFORE internal flags. Per
    /// PR-E R11 gemini HIGH: user `extra_args` land first, then the
    /// internal flags (`-json`, `-count=1`, `-run`) the parser needs,
    /// so a user who supplies `-v=false` or a conflicting reporter
    /// flag can't silently break output parsing.
    ///
    /// Note: `go test` does NOT use last-flag-wins for all flags.
    /// Duplicating `-json` or `-run` produces "flag provided twice"
    /// errors from Go's stdlib flag package. Users who supply those
    /// flags explicitly will see the error surfaced via
    /// `Discovery(..)` rather than silent output corruption —
    /// louder failure mode than PR-E/F.
    pub extra_args: Vec<String>,
}

#[async_trait]
impl TestRunner for GoTestRunner {
    fn name(&self) -> &'static str {
        "gotest"
    }

    async fn run(&self, repo_root: &Path, plan: &TestPlan) -> Result<TestRunSummary, ImpactError> {
        if plan.is_empty() {
            return Ok(TestRunSummary::default());
        }
        // Group tests by package so we issue one `go test -json -run`
        // per package. BTreeMap for deterministic iteration (matters
        // for the integration test's ordering asserts).
        //
        // **Parallel execution deferred to v2.2** (gemini MED on
        // PR #26 R0). Running packages sequentially is
        // `N_packages × per-package-runtime` on the wall clock. A
        // `join_all` / concurrent-stream rewrite would cut that to
        // `max(per-package-runtime) + dispatch overhead`, a real win
        // when plans span many packages. Reasons to defer:
        //
        // 1. v2.1 plans are single-digit packages typically (selector
        //    emits ≤100 tests, batched by parent-dir ≈ 1-3 packages).
        //    Serial cost is ~1-5s total — below user-perceptible
        //    friction. The parallel version pays a complexity tax
        //    (tokio::spawn per pkg, ordered `results` merge, error
        //    propagation across tasks) that isn't earning its keep.
        // 2. `go test` itself already parallelizes within a package
        //    via `t.Parallel()`. Cross-package parallelism mainly
        //    helps when each package's test suite is fast-but-numerous,
        //    which is a pattern we haven't seen in dogfood yet.
        // 3. Revisit trigger: dogfood eval seed (PR-J) will measure
        //    real plan shapes; if avg packages-per-plan exceeds 4 OR
        //    p95 wall-time exceeds 10s, rewrite with a bounded-
        //    concurrency stream (not naive join_all — that lets a
        //    slow package starve `results` ordering).
        let mut by_pkg: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for t in &plan.tests {
            if let Some((pkg, name)) = t.as_str().split_once(ID_SEP) {
                by_pkg.entry(pkg).or_default().push(name);
            }
        }

        let mut results: Vec<TestRunResult> = Vec::new();
        for (pkg, names) in by_pkg {
            // `^(A|B|C)$` filter — `go test -run` takes a regex, so
            // anchoring with `^...$` prevents partial-name matches
            // (e.g. `-run TestFoo` would also run `TestFooBar`).
            let filter = format!("^({})$", names.join("|"));

            let mut cmd = Command::new("go");
            cmd.arg("test");
            // PR-E R11 argv-precedence: user extra_args FIRST,
            // internal flags LAST. See `extra_args` doc for the
            // nuance (go test duplicates error instead of last-wins).
            for a in &self.extra_args {
                cmd.arg(a);
            }
            cmd.arg("-json")
                // `-count=1` disables go test's build/result cache.
                // Without this, repeat runs on unchanged sources
                // return cached results with `Elapsed: 0` and no
                // re-execution — turn validation would silently fake
                // green on broken impls the user meant to verify.
                .arg("-count=1")
                .arg("-run")
                .arg(&filter)
                // Package path is positional; use the canonical
                // import path from the test id. `go test` resolves
                // it relative to the current module / GOPATH.
                .arg(pkg);
            let out = cmd
                .current_dir(repo_root)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
                .map_err(|e| ImpactError::Backend(Box::new(GoTestError::Io(e))))?;
            let stdout_text = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr_text = String::from_utf8_lossy(&out.stderr).to_string();

            let per_test = parse_run_ndjson(&stdout_text);

            // Surface build failures / catastrophic go-tool errors
            // as Discovery. Build-fail events emit no per-test
            // outcomes, so `per_test` stays empty and every plan id
            // would otherwise silently degrade to Unknown — making
            // the caller unable to distinguish "tests not run" from
            // "tests ran and passed".
            //
            // R1 gemini HIGH + codex P1 on PR #26: Go returns exit
            // code 1 for BOTH test failures AND build failures. My
            // R0 gate `!= Some(1)` skipped the error path in the
            // build-fail case (empty per_test + exit=1 → gate
            // bypassed → all plan ids silently became Unknown).
            // Verified empirically with an `undefined: X` fixture:
            // stdout carries `build-output` + `build-fail` +
            // package-level `fail` events, no `Test` field
            // anywhere, exit code = 1.
            //
            // Fix: any non-zero exit with empty per_test is a
            // Discovery error. Test-failure case stays correct
            // because failing tests DO emit per-test events, so
            // per_test is non-empty and this gate is skipped.
            let exit_code = out.status.code();
            if per_test.is_empty() && exit_code != Some(0) {
                // Cap the error body — sibling site to
                // `discover_go_tests` (R3 gemini MED on PR #26).
                // Build-fail dumps in a bulky package can exceed MB.
                let merged =
                    cap_to_max_bytes(merge_pipes(&stdout_text, &stderr_text), MAX_DETAIL_BYTES);
                return Err(ImpactError::Backend(Box::new(GoTestError::Discovery(
                    format!(
                        "go test on `{pkg}` exited {} and produced no \
                         per-test events:\n{}",
                        exit_code
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "<signal>".into()),
                        merged
                    ),
                ))));
            }

            for name in names {
                let id = TestId::new(format!("{pkg}{ID_SEP}{name}"));
                let (outcome, duration_ms, detail) =
                    per_test
                        .get(name)
                        .cloned()
                        .unwrap_or((TestOutcome::Unknown, 0, String::new()));
                results.push(TestRunResult {
                    id,
                    outcome,
                    duration_ms,
                    detail: truncate_detail(detail),
                });
            }
        }
        Ok(TestRunSummary { results })
    }
}

/// Parse `go test -json` NDJSON output into per-test outcomes. The
/// returned map is keyed by the bare test name (no package prefix)
/// because each invocation is package-scoped — collisions across
/// packages are structurally impossible.
///
/// Per-test `detail` is accumulated from `output` events that carry
/// the test's name. Final `pass`/`fail`/`skip` event seals the outcome
/// and records `Elapsed` (Go emits seconds as f64; we convert to ms).
///
/// **Subtest aggregation** (gemini HIGH on PR #26 R0): Go's `t.Run`
/// emits events with `Test="Parent/Sub"`. The caller looks up results
/// by the top-level name (from `-list`, which only emits top-level),
/// so subtest `output` events (which carry the actual failure text
/// `    test.go:7: sub boom`) would be invisible to the TUI if we only
/// keyed on the full name. Fix: for every event, ALSO append its
/// `output` into the top-level parent's slot. Outcome events
/// (`pass`/`fail`/`skip`) stay scoped to their own test — parent
/// outcomes are sealed by Go's own parent-level event that always
/// fires after the last subtest.
///
/// Both slots are still populated independently, so a future caller
/// that wants subtest-granular results can look up `TestParent/Sub`
/// and get exactly that subtest's scoped output + outcome. The
/// parent's slot is a superset, not a substitute.
///
/// **Two-pass structure** (R3 gemini MED on PR #26): the naive
/// "walk ancestors per output event" shape allocates a `String`
/// per level per event, which adds up fast on verbose test output
/// or deep subtest trees. R3 splits into (a) a single-pass accumulator
/// keyed on the event's own `Test` field, then (b) a finalize pass
/// that rolls each accumulated per-test output into its ancestors
/// — one `to_string()` allocation per ancestor, not per event.
/// Semantically identical; locked by the existing
/// `parse_run_ndjson_aggregates_subtest_output_into_parent` +
/// `parse_run_ndjson_aggregates_nested_subtest_into_every_ancestor`
/// tests.
fn parse_run_ndjson(text: &str) -> HashMap<String, (TestOutcome, u64, String)> {
    // First pass: accumulate each event into the specific slot named
    // by its own `Test` field. No ancestor walk.
    let mut state: HashMap<String, (TestOutcome, u64, String)> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let ev: GoTestEvent = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        // Per-test events always have `Test` set. Package-level
        // events (no `Test`) are ignored for per-test rollup.
        // Move the owned `String` directly out of the Option instead
        // of cloning (R4 gemini MED on PR #26) — ev is not reused.
        let test = match ev.test {
            Some(t) if !t.is_empty() => t,
            _ => continue,
        };
        let slot = state
            .entry(test)
            .or_insert((TestOutcome::Unknown, 0, String::new()));
        match ev.action.as_str() {
            "output" => {
                if let Some(o) = ev.output {
                    slot.2.push_str(&o);
                }
            }
            "pass" => {
                slot.0 = TestOutcome::Pass;
                slot.1 = elapsed_to_ms(ev.elapsed);
            }
            "fail" => {
                slot.0 = TestOutcome::Fail;
                slot.1 = elapsed_to_ms(ev.elapsed);
            }
            "skip" => {
                slot.0 = TestOutcome::Skip;
                slot.1 = elapsed_to_ms(ev.elapsed);
            }
            _ => {} // "run", "pause", "cont", "bench" — ignored.
        }
    }
    // Cap each slot's accumulated output BEFORE the snapshot so the
    // subtest-outputs clone + rollup work on bounded strings
    // (R4 gemini MED on PR #26). A verbose test emitting MB of
    // `t.Logf` lines would otherwise balloon the slot, then get
    // cloned wholesale into `subtest_outputs`, then get pushed
    // into each ancestor — all before any truncation. Capping
    // once here keeps peak memory at ~(unique_tests × MAX_DETAIL_BYTES)
    // instead of ~(unique_tests × raw_stdout_bytes).
    for slot in state.values_mut() {
        cap_in_place(&mut slot.2, MAX_DETAIL_BYTES);
    }

    // Second pass: roll each subtest's accumulated output up the
    // `/`-separated ancestor chain so TestA/B/C → TestA/B, TestA.
    // Snapshot subtest keys + contents first so we can mutate the
    // map while iterating. Only the output (String) is cloned; the
    // outcome + elapsed for each ancestor are sealed by that
    // ancestor's own terminal event (emitted by Go at every level
    // that runs), so we never copy outcomes upward.
    //
    // Non-subtest entries (no `/` in the key) are skipped — they
    // have no ancestors, and they receive rolled-up output from
    // any descendants that DO have `/` in their key.
    let subtest_outputs: Vec<(String, String)> = state
        .iter()
        .filter(|(k, _)| k.contains('/'))
        .map(|(k, v)| (k.clone(), v.2.clone()))
        .collect();
    for (test, output) in subtest_outputs {
        let mut ancestor = test.as_str();
        while let Some((prefix, _)) = ancestor.rsplit_once('/') {
            state
                .entry(prefix.to_string())
                .or_insert((TestOutcome::Unknown, 0, String::new()))
                .2
                .push_str(&output);
            ancestor = prefix;
        }
    }
    // Ancestors accumulated from multiple subtests may now exceed the
    // cap again — cap once more after the rollup to enforce the
    // bound across the whole map.
    for slot in state.values_mut() {
        cap_in_place(&mut slot.2, MAX_DETAIL_BYTES);
    }
    state
}

/// Convert Go's `Elapsed` (seconds, f64) to milliseconds (u64).
/// Negative / NaN / infinite values clamp to 0 rather than panic in
/// `as u64` — f64-to-int conversion is UB-adjacent on the edges.
fn elapsed_to_ms(elapsed: Option<f64>) -> u64 {
    match elapsed {
        Some(e) if e.is_finite() && e >= 0.0 => (e * 1000.0).round() as u64,
        _ => 0,
    }
}

/// Per-test detail truncation to `MAX_DETAIL_BYTES`. Empty input →
/// `None` so the TUI knows to hide the detail pane entirely. UTF-8
/// char-boundary walk-back matches the pytest/jest pattern.
fn truncate_detail(text: String) -> Option<Arc<str>> {
    if text.is_empty() {
        return None;
    }
    Some(Arc::<str>::from(cap_to_max_bytes(text, MAX_DETAIL_BYTES)))
}

/// Truncate `text` to at most `max` bytes, walking back to the nearest
/// UTF-8 char boundary. Shared helper so the Discovery error path
/// (`discover_go_tests` + runner `Discovery(..)` branch) and the
/// per-test-detail path (`truncate_detail`) both cap unbounded output
/// the same way. R3 gemini MED on PR #26: discovery error merged
/// stdout + stderr without any limit, so a workspace with heavy build
/// errors could dump megabytes into a single error string that the TUI
/// would then try to render.
///
/// Walk-back terminates in ≤3 iterations because UTF-8 codepoints are
/// ≤4 bytes; every byte boundary is either a char boundary or at most
/// 3 bytes inside a 4-byte codepoint.
fn cap_to_max_bytes(mut text: String, max: usize) -> String {
    cap_in_place(&mut text, max);
    text
}

/// In-place variant of [`cap_to_max_bytes`]. Used by
/// [`parse_run_ndjson`] between passes to bound each slot's
/// accumulated output — R4 gemini MED on PR #26: verbose test output
/// (a `t.Logf` loop emitting MB of lines) would otherwise balloon the
/// per-slot `String` and the R3 two-pass snapshot would then clone
/// ALL of it into `subtest_outputs` before truncation ever ran.
/// Capping in-place before the snapshot bounds the clone to
/// `MAX_DETAIL_BYTES`.
///
/// Walk-back terminates in ≤3 iterations (UTF-8 codepoints are ≤4
/// bytes) so this is O(1) per call regardless of the input length.
fn cap_in_place(text: &mut String, max: usize) {
    if text.len() > max {
        let mut cutoff = max;
        while !text.is_char_boundary(cutoff) {
            cutoff -= 1;
        }
        text.truncate(cutoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_test_name_line_accepts_canonical_prefixes() {
        assert!(is_test_name_line("TestFoo"));
        assert!(is_test_name_line("ExampleBaz"));
        assert!(is_test_name_line("FuzzQux"));
        // Subtest-style underscore (rare but legal).
        assert!(is_test_name_line("Test_foo"));
        // Digit after prefix (some test-generator conventions).
        assert!(is_test_name_line("Test2"));
    }

    #[test]
    fn is_test_name_line_accepts_unicode_uppercase_tails() {
        // R4 codex P2 on PR #26: Go's `testing.matchTests` uses
        // `!unicode.IsLower(first_rune)`, so non-ASCII uppercase
        // letters after the prefix must be accepted. Verified
        // empirically with go 1.25.6: `func TestÉclair(t *testing.T)`
        // is listed by `go test -list`. My R3 ASCII-only check
        // dropped these from discovery.
        assert!(is_test_name_line("TestÉclair"));
        assert!(is_test_name_line("ExampleÖ"));
        assert!(is_test_name_line("FuzzΑ")); // Greek capital alpha.
    }

    #[test]
    fn is_test_name_line_rejects_unicode_lowercase_tails() {
        // Symmetric guard: a LOWERCASE Unicode letter after the
        // prefix must be rejected, matching Go's own behaviour. Go's
        // test matcher reads these as continuation of a prose word,
        // not a test function name.
        assert!(!is_test_name_line("Testéclair"));
        assert!(!is_test_name_line("Testα")); // Greek lowercase alpha.
    }

    #[test]
    fn is_test_name_line_accepts_bare_example() {
        // R1 codex P2 on PR #26: `func Example()` is a valid
        // package-level runnable example. `go test -list` emits the
        // bare `Example` name; my R0 parser rejected it because the
        // tail-char rule required a suffix. Verified empirically
        // with go 1.25.6: a package containing a bare `Example()`
        // with valid `// Output:` comment lists cleanly as `Example`.
        assert!(is_test_name_line("Example"));
    }

    #[test]
    fn is_test_name_line_rejects_bare_test_and_fuzz() {
        // Unlike Example, bare `Test` / `Fuzz` are not valid test
        // function names — Go's testing package requires a tail char
        // for those. Rejecting them protects against false positives
        // on prose that happens to start with `Test`/`Fuzz`.
        assert!(!is_test_name_line("Test"));
        assert!(!is_test_name_line("Fuzz"));
    }

    #[test]
    fn is_test_name_line_rejects_benchmarks() {
        // R1 codex P2 on PR #26: `go test -run` does NOT execute
        // benchmarks (only `-bench` does per `go help testflag`).
        // My R0 discovery included `BenchmarkXxx` in the universe,
        // which meant the runner would then report every selected
        // benchmark id as `Unknown` with stdout "no tests to run"
        // (verified with go 1.25.6 on PR #26). Until v2.2 adds a
        // `-bench`-aware runner path, benchmarks are excluded from
        // discovery entirely.
        assert!(!is_test_name_line("BenchmarkFoo"));
        assert!(!is_test_name_line("Benchmark"));
    }

    #[test]
    fn is_test_name_line_rejects_non_test_prose() {
        // `ok PKG 0.001s [no tests to run]` — the tail line go emits.
        assert!(!is_test_name_line("ok  \texample.com/probe\t0.003s"));
        // Prefix-without-tail-capital: "Testing" is prose, not a test.
        assert!(!is_test_name_line("Testing helper function"));
        // Lowercase tail: Go's test matcher rejects these too.
        assert!(!is_test_name_line("Testx"));
        // Empty.
        assert!(!is_test_name_line(""));
    }

    #[test]
    fn parse_list_ndjson_extracts_named_tests_with_package_prefix() {
        // Canonical `go test -json -list .*` stream (abbreviated).
        // `BenchmarkGamma` MUST NOT appear in the universe — R1
        // codex P2 on PR #26: benchmarks need `-bench` not `-run`,
        // so including them in discovery just pollutes the plan
        // with ids the runner can't execute.
        // `Example` (bare) MUST appear — R1 codex P2 on PR #26: it's
        // a first-class package-level example.
        let ndjson = r#"{"Action":"start","Package":"example.com/foo"}
{"Action":"output","Package":"example.com/foo","Output":"TestAlpha\n"}
{"Action":"output","Package":"example.com/foo","Output":"TestBeta\n"}
{"Action":"output","Package":"example.com/foo","Output":"Example\n"}
{"Action":"output","Package":"example.com/foo","Output":"ExampleFoo\n"}
{"Action":"output","Package":"example.com/foo","Output":"FuzzBaz\n"}
{"Action":"output","Package":"example.com/foo","Output":"ok  \texample.com/foo\t0.003s\n"}
{"Action":"pass","Package":"example.com/foo","Elapsed":0.004}
{"Action":"start","Package":"example.com/bar"}
{"Action":"output","Package":"example.com/bar","Output":"BenchmarkGamma\n"}
{"Action":"pass","Package":"example.com/bar","Elapsed":0.001}"#;
        let uni = parse_list_ndjson(ndjson);
        let ids: Vec<String> = uni.tests.iter().map(|t| t.as_str().to_string()).collect();
        assert_eq!(
            ids,
            vec![
                "example.com/foo::TestAlpha".to_string(),
                "example.com/foo::TestBeta".to_string(),
                "example.com/foo::Example".to_string(),
                "example.com/foo::ExampleFoo".to_string(),
                "example.com/foo::FuzzBaz".to_string(),
                // BenchmarkGamma deliberately absent — see comment.
            ],
            "benchmarks must be excluded + bare Example must be present"
        );
    }

    #[test]
    fn parse_list_ndjson_dedupes_identical_ids() {
        // Guard against a Go-side quirk where re-running -list on a
        // hot module cache could (in theory) emit duplicate output
        // events. The universe must stay deduped.
        let ndjson = r#"{"Action":"output","Package":"example.com/foo","Output":"TestAlpha\n"}
{"Action":"output","Package":"example.com/foo","Output":"TestAlpha\n"}"#;
        let uni = parse_list_ndjson(ndjson);
        assert_eq!(uni.len(), 1);
    }

    #[test]
    fn parse_list_ndjson_tolerates_non_json_noise() {
        // The `go` driver can emit plain text lines outside the JSON
        // envelope on catastrophic failures (e.g. build errors that
        // precede test2json activation). Parsing must not fail hard.
        let ndjson = r#"# pre-JSON noise from go tooling
{"Action":"output","Package":"example.com/foo","Output":"TestAlpha\n"}
Actual JSON error: extra } somewhere
}}}"#;
        let uni = parse_list_ndjson(ndjson);
        assert_eq!(uni.len(), 1);
        assert_eq!(uni.tests[0].as_str(), "example.com/foo::TestAlpha");
    }

    #[test]
    fn parse_list_ndjson_skips_summary_and_package_events() {
        // Non-output events and summary/ok lines must not become test
        // ids. Only bare `TestXxx`/`BenchmarkXxx`/`ExampleXxx`/`FuzzXxx`
        // line contents count.
        let ndjson = r#"{"Action":"start","Package":"example.com/foo"}
{"Action":"output","Package":"example.com/foo","Output":"ok  \texample.com/foo\t0.003s\n"}
{"Action":"output","Package":"example.com/foo","Output":"FAIL\texample.com/foo\t[build failed]\n"}
{"Action":"pass","Package":"example.com/foo","Elapsed":0.004}"#;
        let uni = parse_list_ndjson(ndjson);
        assert!(uni.is_empty(), "no test lines should be extracted: {uni:?}");
    }

    #[test]
    fn parse_run_ndjson_rolls_up_pass_fail_skip_per_test() {
        // Condensed real `go test -json` output for a three-test run:
        // TestAlpha passes, TestBeta is skipped, TestGamma fails.
        let ndjson = r#"{"Action":"run","Package":"p","Test":"TestAlpha"}
{"Action":"output","Package":"p","Test":"TestAlpha","Output":"=== RUN   TestAlpha\n"}
{"Action":"output","Package":"p","Test":"TestAlpha","Output":"--- PASS: TestAlpha (0.00s)\n"}
{"Action":"pass","Package":"p","Test":"TestAlpha","Elapsed":0.002}
{"Action":"run","Package":"p","Test":"TestBeta"}
{"Action":"output","Package":"p","Test":"TestBeta","Output":"--- SKIP: TestBeta (0.00s)\n"}
{"Action":"skip","Package":"p","Test":"TestBeta","Elapsed":0}
{"Action":"run","Package":"p","Test":"TestGamma"}
{"Action":"output","Package":"p","Test":"TestGamma","Output":"    test.go:7: boom\n"}
{"Action":"output","Package":"p","Test":"TestGamma","Output":"--- FAIL: TestGamma (0.00s)\n"}
{"Action":"fail","Package":"p","Test":"TestGamma","Elapsed":0.001}
{"Action":"output","Package":"p","Output":"FAIL\n"}
{"Action":"fail","Package":"p","Elapsed":0.005}"#;
        let m = parse_run_ndjson(ndjson);
        let alpha = m.get("TestAlpha").expect("alpha");
        assert_eq!(alpha.0, TestOutcome::Pass);
        assert_eq!(alpha.1, 2);
        assert!(alpha.2.contains("PASS: TestAlpha"));
        let beta = m.get("TestBeta").expect("beta");
        assert_eq!(beta.0, TestOutcome::Skip);
        assert_eq!(beta.1, 0);
        let gamma = m.get("TestGamma").expect("gamma");
        assert_eq!(gamma.0, TestOutcome::Fail);
        assert_eq!(gamma.1, 1);
        assert!(
            gamma.2.contains("boom"),
            "failed test detail must carry source log: {:?}",
            gamma.2
        );
    }

    #[test]
    fn parse_run_ndjson_ignores_package_level_events() {
        // Events lacking `Test` (package-level pass/fail/output)
        // must not create phantom entries.
        let ndjson = r#"{"Action":"start","Package":"p"}
{"Action":"output","Package":"p","Output":"FAIL\n"}
{"Action":"fail","Package":"p","Elapsed":0.005}"#;
        let m = parse_run_ndjson(ndjson);
        assert!(m.is_empty());
    }

    #[test]
    fn parse_run_ndjson_aggregates_subtest_output_into_parent() {
        // R1 gemini HIGH on PR #26: `t.Run` emits subtest events
        // with `Test="Parent/Sub"`. The `TestPlan` only carries
        // top-level names (from `-list`), so subtest `output` events
        // — which carry the ACTUAL failure text (`sub_test.go:7: sub
        // boom`) — were dropped when the runner looked up results
        // by the top-level name. Fix: append subtest output into the
        // parent's slot too, so the top-level lookup sees the full
        // forensic trace.
        //
        // Captured verbatim from go 1.25.6 on PR #26 verification:
        // TestParent with two subtests Sub1 (pass) and Sub2 (fail),
        // where Sub2 calls `t.Fatal("sub boom")`.
        let ndjson = r#"{"Action":"run","Package":"p","Test":"TestParent"}
{"Action":"output","Package":"p","Test":"TestParent","Output":"=== RUN   TestParent\n"}
{"Action":"run","Package":"p","Test":"TestParent/Sub1"}
{"Action":"output","Package":"p","Test":"TestParent/Sub1","Output":"=== RUN   TestParent/Sub1\n"}
{"Action":"output","Package":"p","Test":"TestParent/Sub1","Output":"--- PASS: TestParent/Sub1 (0.00s)\n"}
{"Action":"pass","Package":"p","Test":"TestParent/Sub1","Elapsed":0}
{"Action":"run","Package":"p","Test":"TestParent/Sub2"}
{"Action":"output","Package":"p","Test":"TestParent/Sub2","Output":"    sub_test.go:7: sub boom\n"}
{"Action":"output","Package":"p","Test":"TestParent/Sub2","Output":"--- FAIL: TestParent/Sub2 (0.00s)\n"}
{"Action":"fail","Package":"p","Test":"TestParent/Sub2","Elapsed":0}
{"Action":"output","Package":"p","Test":"TestParent","Output":"--- FAIL: TestParent (0.00s)\n"}
{"Action":"fail","Package":"p","Test":"TestParent","Elapsed":0.003}"#;
        let m = parse_run_ndjson(ndjson);

        // Parent's outcome sealed by Go's parent-level `fail` event.
        let parent = m.get("TestParent").expect("parent must be present");
        assert_eq!(parent.0, TestOutcome::Fail);
        assert_eq!(parent.1, 3);
        // Parent detail must carry the SUBTEST failure text — that's
        // the regression this test locks.
        assert!(
            parent.2.contains("sub boom"),
            "parent detail missing subtest failure text: {:?}",
            parent.2
        );
        assert!(
            parent.2.contains("--- FAIL: TestParent/Sub2"),
            "parent detail missing subtest fail marker: {:?}",
            parent.2
        );
        // And it must still carry the parent's own events.
        assert!(
            parent.2.contains("=== RUN   TestParent\n"),
            "parent detail missing its own run marker: {:?}",
            parent.2
        );

        // Subtest slots are still populated independently — a
        // future caller doing subtest-granular lookup gets accurate
        // scoped results. Sub1 passes, Sub2 fails.
        let sub1 = m.get("TestParent/Sub1").expect("sub1 must be present");
        assert_eq!(sub1.0, TestOutcome::Pass);
        let sub2 = m.get("TestParent/Sub2").expect("sub2 must be present");
        assert_eq!(sub2.0, TestOutcome::Fail);
        assert!(
            sub2.2.contains("sub boom"),
            "subtest-scoped detail must contain its own failure text"
        );
    }

    #[test]
    fn parse_run_ndjson_aggregates_nested_subtest_into_every_ancestor() {
        // R2 gemini MED on PR #26: R1 only rolled up output to the
        // top-most parent (split_once('/') gave `TestA` from
        // `TestA/B/C`). Nested subtests like `TestA/B/C` left the
        // intermediate `TestA/B` slot empty — fine for v2.1 (only
        // top-level ids are in the universe), but a future feature
        // that looks up intermediate nodes would see blank details.
        // Fix walks the full ancestor chain so every `/`-separated
        // prefix gets the output appended.
        let ndjson = r#"{"Action":"run","Package":"p","Test":"TestA/B/C"}
{"Action":"output","Package":"p","Test":"TestA/B/C","Output":"    test.go:7: deep boom\n"}
{"Action":"output","Package":"p","Test":"TestA/B/C","Output":"--- FAIL: TestA/B/C (0.00s)\n"}
{"Action":"fail","Package":"p","Test":"TestA/B/C","Elapsed":0}"#;
        let m = parse_run_ndjson(ndjson);

        // Leaf slot carries its own output.
        assert!(
            m.get("TestA/B/C")
                .map(|e| e.2.contains("deep boom"))
                .unwrap_or(false),
            "leaf slot must carry subtest output"
        );
        // Intermediate slot (TestA/B) carries the same output.
        assert!(
            m.get("TestA/B")
                .map(|e| e.2.contains("deep boom"))
                .unwrap_or(false),
            "intermediate ancestor TestA/B must carry nested subtest output"
        );
        // Top-level slot (TestA) also carries it.
        assert!(
            m.get("TestA")
                .map(|e| e.2.contains("deep boom"))
                .unwrap_or(false),
            "top-level ancestor TestA must carry nested subtest output"
        );
    }

    #[test]
    fn parse_run_ndjson_caps_per_slot_output_to_max_detail_bytes() {
        // R4 gemini MED on PR #26: verbose test output must not
        // balloon the per-slot `String` and then get cloned wholesale
        // into `subtest_outputs`. Synthesise a stream with output
        // ≥ 2× MAX_DETAIL_BYTES and confirm the post-parse slot is
        // bounded.
        //
        // Build a single-test, single-event line with a payload that
        // vastly exceeds the cap.
        let huge = "x".repeat(MAX_DETAIL_BYTES * 3);
        let ndjson = format!(
            r#"{{"Action":"run","Package":"p","Test":"TestHuge"}}
{{"Action":"output","Package":"p","Test":"TestHuge","Output":"{huge}"}}
{{"Action":"pass","Package":"p","Test":"TestHuge","Elapsed":0}}"#
        );
        let m = parse_run_ndjson(&ndjson);
        let slot = m.get("TestHuge").expect("TestHuge must be present");
        assert!(
            slot.2.len() <= MAX_DETAIL_BYTES,
            "slot output must be capped to MAX_DETAIL_BYTES; got {}",
            slot.2.len()
        );
    }

    #[test]
    fn cap_in_place_matches_cap_to_max_bytes_behaviour() {
        // Single-source-of-truth check — the owned + borrow-mut
        // variants must produce byte-identical output. R4 gemini MED
        // extracted cap_in_place as the primitive so parse_run_ndjson
        // can cap slots without a take+replace cycle.
        let mut s1 = "aaaaaaabcdéé".to_string();
        let len = s1.len();
        let s2 = cap_to_max_bytes(s1.clone(), len - 1);
        cap_in_place(&mut s1, len - 1);
        assert_eq!(s1, s2);
    }

    #[test]
    fn elapsed_to_ms_clamps_weird_values() {
        assert_eq!(elapsed_to_ms(None), 0);
        assert_eq!(elapsed_to_ms(Some(f64::NAN)), 0);
        assert_eq!(elapsed_to_ms(Some(f64::INFINITY)), 0);
        assert_eq!(elapsed_to_ms(Some(-1.0)), 0);
        // Normal case: rounded-half-to-nearest.
        assert_eq!(elapsed_to_ms(Some(0.1234)), 123);
        assert_eq!(elapsed_to_ms(Some(0.0)), 0);
    }

    #[test]
    fn truncate_detail_empty_returns_none() {
        assert!(truncate_detail(String::new()).is_none());
    }

    #[test]
    fn truncate_detail_caps_at_max_and_walks_to_char_boundary() {
        let mut s = "a".repeat(MAX_DETAIL_BYTES - 2);
        s.push('é');
        s.push('é');
        let out = truncate_detail(s).expect("non-empty must return Some");
        assert!(out.len() <= MAX_DETAIL_BYTES);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn cap_to_max_bytes_caps_and_walks_to_char_boundary() {
        // R3 gemini MED on PR #26: helper extracted so the
        // Discovery error path + truncate_detail share a single
        // char-boundary-safe truncation implementation. Test locks
        // the contract in isolation (independent of truncate_detail's
        // Option-return wrapping).
        let mut s = "a".repeat(1022);
        s.push('é'); // 2 bytes, lands at byte 1024
        s.push('é'); // 2 bytes, lands past max=1024
        let out = cap_to_max_bytes(s, 1024);
        assert!(out.len() <= 1024);
        // Output valid UTF-8 — implicit since String can't carry invalid.
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn cap_to_max_bytes_passes_through_short_input() {
        let s = String::from("hello");
        assert_eq!(cap_to_max_bytes(s, 1024), "hello");
    }

    #[test]
    fn cap_to_max_bytes_handles_empty() {
        let s = String::new();
        assert_eq!(cap_to_max_bytes(s, 1024), "");
    }

    #[test]
    fn merge_pipes_handles_empty_either_side() {
        assert_eq!(merge_pipes("out", ""), "out");
        assert_eq!(merge_pipes("", "err"), "err");
        assert_eq!(merge_pipes("out", "err"), "out\nerr");
        assert_eq!(merge_pipes("", ""), "");
    }
}
