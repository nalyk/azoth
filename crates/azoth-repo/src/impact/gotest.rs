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
    pub async fn discover(repo_root: PathBuf) -> Result<Self, ImpactError> {
        match Self::detect(&repo_root) {
            Ok(Some(_)) => {
                let universe = discover_go_tests(&repo_root).await?;
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
        for path in &diff.changed_files {
            // Go-native heuristic: parent-dir-name against package path.
            // See module doc for why this differs from jest/pytest's
            // file-stem approach.
            let parent_name = parent_dir_name(path);
            if parent_name.is_empty() {
                continue;
            }
            for t in &self.universe.tests {
                let t_str = t.as_str();
                // Package path is everything left of `::`. Missing
                // separator means the universe entry is malformed —
                // skip rather than crash (defensive; discovery emits
                // the separator on every id).
                let pkg_path = t_str.split_once(ID_SEP).map(|(p, _)| p).unwrap_or("");
                if word_boundary_contains(pkg_path, parent_name) && seen.insert(t_str) {
                    plan.tests.push(t.clone());
                    plan.rationale
                        .push(format!("changed file {path} → pkg dir {parent_name}"));
                    plan.confidence.push(1.0);
                }
            }
        }
        debug_assert!(plan.is_well_formed());
        Ok(plan)
    }
}

/// Return the last component of `path`'s parent directory, or empty
/// string when `path` is a bare filename or has no meaningful parent.
/// `src/auth/tokens.go` → `"auth"`, `foo.go` → `""`,
/// `/root/foo.go` → `""` (root has no named parent).
fn parent_dir_name(path: &str) -> &str {
    Path::new(path)
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("")
}

/// Shell out to `go test -json -list '.*' ./...` inside `repo_root`
/// and parse the emitted NDJSON events. Each `output` event carrying
/// a bare `TestXxx`/`BenchmarkXxx`/`ExampleXxx` line is a listed test
/// in the package named by the event's `Package` field.
///
/// Non-zero exit → `GoTestError::Discovery` with stdout+stderr merged.
pub async fn discover_go_tests(repo_root: &Path) -> Result<TestUniverse, ImpactError> {
    let out = Command::new("go")
        .arg("test")
        .arg("-json")
        .arg("-list")
        .arg(".*")
        .arg("./...")
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| ImpactError::Backend(Box::new(GoTestError::Io(e))))?;
    if !out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        return Err(ImpactError::Backend(Box::new(GoTestError::Discovery(
            merge_pipes(&stdout, &stderr),
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

/// True when `line` matches a Go test function name — `TestFoo`,
/// `BenchmarkFoo`, or `ExampleFoo` with a Go-identifier-start char
/// after the prefix. Looser checks like `starts_with("Test")` would
/// accept `"Testing helper: ..."` or `"TestMain with "` (the magic
/// function) which aren't real test listings.
fn is_test_name_line(line: &str) -> bool {
    for prefix in ["Test", "Benchmark", "Example", "Fuzz"] {
        if let Some(tail) = line.strip_prefix(prefix) {
            // `TestFoo` requires the char after `Test` to be an
            // uppercase letter, underscore, or digit (Go test naming
            // convention — see testing.matchTests). `Test` alone or
            // `Testx` (lowercase) are rejected by `go test` itself.
            // Underscore permitted for subtest table names (rare but
            // legal).
            if let Some(c) = tail.chars().next() {
                if c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit() {
                    return true;
                }
            }
            return false;
        }
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
            // "tests ran and passed". Exit 0/1 are the sanctioned
            // pass/any-fail codes; anything else signals a toolchain
            // problem (see `cmd/go` source: errExitCode uses 2 for
            // usage errors, 1 for test failure).
            let exit_code = out.status.code();
            if per_test.is_empty() && exit_code != Some(0) && exit_code != Some(1) {
                return Err(ImpactError::Backend(Box::new(GoTestError::Discovery(
                    format!(
                        "go test on `{pkg}` exited {} and produced no \
                         per-test events:\n{}",
                        exit_code
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "<signal>".into()),
                        merge_pipes(&stdout_text, &stderr_text)
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
fn parse_run_ndjson(text: &str) -> HashMap<String, (TestOutcome, u64, String)> {
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
        let test = match ev.test {
            Some(ref t) if !t.is_empty() => t.clone(),
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
fn truncate_detail(mut text: String) -> Option<Arc<str>> {
    if text.is_empty() {
        return None;
    }
    if text.len() > MAX_DETAIL_BYTES {
        let mut cutoff = MAX_DETAIL_BYTES;
        while !text.is_char_boundary(cutoff) {
            cutoff -= 1;
        }
        text.truncate(cutoff);
    }
    Some(Arc::<str>::from(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_test_name_line_accepts_canonical_prefixes() {
        assert!(is_test_name_line("TestFoo"));
        assert!(is_test_name_line("BenchmarkBar"));
        assert!(is_test_name_line("ExampleBaz"));
        assert!(is_test_name_line("FuzzQux"));
        // Subtest-style underscore (rare but legal).
        assert!(is_test_name_line("Test_foo"));
        // Digit after prefix (some test-generator conventions).
        assert!(is_test_name_line("Test2"));
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
        // Just the prefix with nothing after.
        assert!(!is_test_name_line("Test"));
    }

    #[test]
    fn parse_list_ndjson_extracts_named_tests_with_package_prefix() {
        // Canonical `go test -json -list .*` stream (abbreviated).
        let ndjson = r#"{"Action":"start","Package":"example.com/foo"}
{"Action":"output","Package":"example.com/foo","Output":"TestAlpha\n"}
{"Action":"output","Package":"example.com/foo","Output":"TestBeta\n"}
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
                "example.com/bar::BenchmarkGamma".to_string(),
            ]
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
    fn parent_dir_name_returns_last_component() {
        assert_eq!(parent_dir_name("src/auth/tokens.go"), "auth");
        assert_eq!(parent_dir_name("pkg/foo_test.go"), "pkg");
        assert_eq!(parent_dir_name("foo.go"), "");
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
    fn merge_pipes_handles_empty_either_side() {
        assert_eq!(merge_pipes("out", ""), "out");
        assert_eq!(merge_pipes("", "err"), "err");
        assert_eq!(merge_pipes("out", "err"), "out\nerr");
        assert_eq!(merge_pipes("", ""), "");
    }
}
