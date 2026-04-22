//! `PytestImpact` — v2.1 Python ecosystem selector.
//!
//! Shape mirrors [`super::cargo::CargoTestImpact`] so the TurnDriver
//! can swap selectors by language without reshape:
//!
//! - `with_universe(repo_root, universe)` — synthetic universe for
//!   integration tests, skips the `pytest --collect-only` shell-out.
//! - `discover(repo_root)` — production entry point; detects a pytest
//!   config, then shells out to `pytest --collect-only -q` to
//!   enumerate the test universe.
//! - `detect(&Path)` — extension-free detector matching the three
//!   canonical pytest configs (`pytest.ini`, `pyproject.toml`
//!   `[tool.pytest.ini_options]`, `setup.cfg` `[tool:pytest]`). No
//!   file globbing — detection is cheap and deterministic.
//!
//! The selector heuristic is **direct filename-stem match** (confidence
//! `1.0`). Symbol-graph and co-edit widening are deferred to v2.2 — see
//! `docs/superpowers/plans/2026-04-21-v2_1-implementation.md` §PR-E.
//!
//! Why the pure selector path takes a pre-materialised `TestUniverse`:
//! shelling out to `pytest` inside `cargo test` is a recipe for flaky
//! CI on hosts without Python. The integration tests feed synthetic
//! universes and exercise the pure heuristic; the live discovery path
//! is covered by the `live-tools`-gated runner test.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use thiserror::Error;
use tokio::process::Command;

use azoth_core::impact::{ImpactError, ImpactSelector};
use azoth_core::schemas::{Contract, Diff, TestId, TestPlan};

use super::cargo::TestUniverse;
use super::runner::{TestOutcome, TestRunResult, TestRunSummary, TestRunner};

/// Selector-impl version. Bump on heuristic changes so replay can
/// detect plan drift without re-running the selector.
pub const PYTEST_IMPACT_VERSION: u32 = 1;

/// Forensic-detail truncation cap. `PytestRunner::run` stashes the
/// combined stdout+stderr into `TestRunResult.detail` so the TUI
/// can render the tail of a failing run; this cap keeps long
/// pytest tracebacks from pinning 4+ KiB per test indefinitely.
/// Matches `MAX_STDERR_BYTES` in `cargo.rs` by value but kept
/// local so the two runners can diverge independently if a future
/// ecosystem needs a different cap (R6 gemini MED — no more
/// magic numbers).
const MAX_DETAIL_BYTES: usize = 4096;

/// Typed error surface for the pytest backend. Boxed into
/// `ImpactError::Backend` at the selector boundary so `azoth-core`
/// stays agnostic to ecosystem-specific failure modes.
#[derive(Debug, Error)]
pub enum PytestError {
    #[error(
        "pytest not detected (no pytest.ini / \
         pyproject.toml [tool.pytest.ini_options] / \
         setup.cfg [tool:pytest])"
    )]
    NotDetected,
    #[error("pytest dependencies unresolved — run `pip install -e .` or equivalent: {0}")]
    DependenciesUnresolved(String),
    #[error("pytest discovery failed: {0}")]
    Discovery(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// v2.1 Python selector. Construction is explicit (no `Default`)
/// because every consumer must commit to `with_universe` (tests) or
/// `discover` (production).
pub struct PytestImpact {
    repo_root: PathBuf,
    universe: TestUniverse,
}

impl PytestImpact {
    /// Construct with an already-materialised universe. Integration
    /// tests feed synthetic universes; production uses `discover`.
    pub fn with_universe(repo_root: PathBuf, universe: TestUniverse) -> Self {
        Self {
            repo_root,
            universe,
        }
    }

    /// Production entry point: detect pytest config, shell out to
    /// `pytest --collect-only -q`, build the universe. Returns
    /// `PytestError::NotDetected` if no recognised config is present.
    pub async fn discover(repo_root: PathBuf) -> Result<Self, ImpactError> {
        if Self::detect(&repo_root).is_none() {
            return Err(ImpactError::Backend(Box::new(PytestError::NotDetected)));
        }
        let universe = discover_pytest_tests(&repo_root).await?;
        Ok(Self {
            repo_root,
            universe,
        })
    }

    /// Extension-free detector. Returns `Some(kind_tag)` when any
    /// recognised pytest config is present; the tag is exposed for
    /// future routing (e.g. "pyproject" configs may want different
    /// defaults than "pytest_ini" ones).
    ///
    /// Sync `std::fs` I/O inside an `async fn` caller chain (see
    /// `discover`) is intentional: these are tiny config files read
    /// once at selector construction, and the `tokio::fs` surface
    /// would add runtime complexity for sub-microsecond gain. If a
    /// future caller needs detection inside a hot async loop, move
    /// this to `tokio::fs::read_to_string` then. Per R1 review from
    /// gemini: the behaviour is acceptable for v2.1.
    pub fn detect(repo_root: &Path) -> Option<&'static str> {
        if repo_root.join("pytest.ini").exists() {
            return Some("pytest_ini");
        }
        // `read_to_string` returns `Err` on missing file, so no
        // preceding `exists()` stat is needed — removing it also
        // closes a TOCTOU window (R1 gemini MED).
        if let Ok(s) = std::fs::read_to_string(repo_root.join("pyproject.toml")) {
            if s.contains("[tool.pytest.ini_options]") {
                return Some("pyproject");
            }
        }
        if let Ok(s) = std::fs::read_to_string(repo_root.join("setup.cfg")) {
            if s.contains("[tool:pytest]") {
                return Some("setup_cfg");
            }
        }
        None
    }

    pub fn universe(&self) -> &TestUniverse {
        &self.universe
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }
}

#[async_trait]
impl ImpactSelector for PytestImpact {
    fn name(&self) -> &'static str {
        "pytest"
    }

    fn version(&self) -> u32 {
        PYTEST_IMPACT_VERSION
    }

    async fn select(&self, diff: &Diff, _contract: &Contract) -> Result<TestPlan, ImpactError> {
        if self.universe.is_empty() || diff.is_empty() {
            return Ok(TestPlan::empty(self.version()));
        }
        let mut plan = TestPlan::empty(self.version());
        // `HashSet<&str>` over `HashSet<TestId>` — the test ids are
        // owned by `self.universe` and live for the whole function,
        // so we can de-dupe with a borrowed &str and skip the
        // per-insert `.clone()` allocation entirely (R2 gemini MED).
        let mut seen: HashSet<&str> = HashSet::new();
        for path in &diff.changed_files {
            let stem = Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if stem.is_empty() {
                continue;
            }
            for t in &self.universe.tests {
                if t.as_str().contains(stem) && seen.insert(t.as_str()) {
                    plan.tests.push(t.clone());
                    plan.rationale
                        .push(format!("changed file {path} → stem {stem}"));
                    plan.confidence.push(1.0);
                }
            }
        }
        debug_assert!(plan.is_well_formed());
        Ok(plan)
    }
}

/// Returns true when either pipe signals a **dependency-level**
/// failure — an import error that occurred during pytest's
/// collection phase (missing package) or at interpreter bootstrap
/// (pytest itself not installed).
///
/// Narrower than a bare `ImportError`/`ModuleNotFoundError`
/// substring scan because R3 codex P2 (`chatgpt-codex-connector`
/// on PR #24) pointed out that a user test body which references
/// those exception types — e.g. `with pytest.raises(ImportError):`
/// or an assertion about `ModuleNotFoundError` messaging — would
/// misfire the broader helper as `DependenciesUnresolved` instead
/// of a real test failure, stealing the per-test diagnosis.
///
/// The narrow signals are:
/// - **pytest collection reporter**: literal wrapper
///   `"ImportError while importing"` appears ONLY in pytest's
///   collection-error section; it is not syntax a user would emit
///   from their own code.
/// - **interpreter bootstrap**: `"No module named 'pytest'"` /
///   `"No module named pytest"` when pytest itself is missing
///   from the environment (covers the `python -m pytest` launch
///   path and direct `pytest` shim failures).
///
/// Ordinary test bodies that `pytest.raises(ImportError)` or
/// fail an assertion about an exception message no longer trip
/// this helper — they get routed through the per-test outcome
/// parser (R3 gemini HIGH) and marked `Fail` like any other test
/// failure.
fn pytest_output_signals_dependency_error(stdout: &str, stderr: &str) -> bool {
    // R6 codex P2 further narrowing: "ImportError while importing"
    // alone STILL appears in user test bodies (e.g.
    // `assert "ImportError while importing" in str(exc)` or a
    // string-equality check on exception messaging). Anchor
    // the signal to pytest's collection-reporter context — the
    // phrase only fires under an `ERROR collecting` section
    // header, which is pytest-specific output that no user test
    // body would emit.
    let is_collection_import_failure =
        |s: &str| s.contains("ERROR collecting") && s.contains("ImportError while importing");
    // R5 codex P2 narrowing: "No module named 'pytest'" on STDOUT
    // could be emitted by a legitimate test body that asserts on
    // the exception message. Bootstrap-level failures where
    // pytest itself isn't installed ALWAYS land on stderr — the
    // Python interpreter prints the traceback to stderr before
    // the terminal reporter starts. Restricting to stderr avoids
    // the stdout false-positive while preserving real-bootstrap
    // detection.
    let is_pytest_missing_bootstrap =
        |s: &str| s.contains("No module named 'pytest'") || s.contains("No module named pytest");
    is_collection_import_failure(stdout)
        || is_collection_import_failure(stderr)
        || is_pytest_missing_bootstrap(stderr)
}

/// Per-test outcome parser over `pytest -v` stdout. R3 gemini HIGH
/// fix for the "one failure sinks all" issue my R2 explicitly
/// called out as pragmatic — gemini escalated it to HIGH for the
/// usability gap, which is the right call.
///
/// `pytest -v` emits one line per test:
///
/// ```text
/// tests/test_foo.py::test_alpha PASSED                         [ 33%]
/// tests/test_foo.py::test_beta  FAILED                         [ 66%]
/// tests/test_foo.py::test_gamma SKIPPED (reason)               [100%]
/// ```
///
/// **Parametrize gotcha** (R4 codex P1): pytest node IDs can
/// contain whitespace inside `[...]` parametrize values — e.g.
/// `test_p.py::test_p[hello world] PASSED`. A naive
/// `split_whitespace()` approach breaks: `test_p[hello` and
/// `world]` become separate tokens and the status token lands
/// at index 2, not 1. Instead we byte-search each line for the
/// canonical ` STATUS` pattern (each status keyword preceded by
/// a space) with a trailing boundary check (whitespace / EOL),
/// then slice the id from `[0..match_idx]`. Preserves all
/// internal whitespace in the test id, including parametrize
/// spaces.
///
/// Statuses beyond the v2.1 core four (`PASSED`, `FAILED`,
/// `SKIPPED`, `ERROR`) map to:
/// - `XFAIL` → `Pass` (expected failure that did fail — a success
///   from the user's point of view)
/// - `XPASS` → `Fail` (expected failure that unexpectedly passed
///   — the test is stale and should be updated)
///
/// Tests that don't appear in the output (pytest didn't schedule
/// them for some reason — config filter, discovery skip) surface
/// as `TestOutcome::Unknown`, NOT guessed as Pass or Fail.
fn parse_pytest_verbose_outcomes(stdout: &str) -> std::collections::HashMap<String, TestOutcome> {
    // R6 codex P2: parse by finding the RIGHTMOST word-boundary-
    // aligned status keyword. Naive left-to-right `find(" STATUS")`
    // mis-matches bracket-embedded occurrences (`test_p[ PASSED ]
    // FAILED` picks the bracketed `PASSED`); naive rsplit-on-last-
    // whitespace mis-matches trailing tokens (`SKIPPED (reason)`
    // picks `reason)`). Right-most word-boundary match solves
    // both: the REAL status always lives at the highest byte
    // position where a status keyword sits on clean whitespace
    // boundaries.
    //
    // pytest `-v` line shape:
    //   `<test_id> <STATUS>[(reason)] [<N>%]`
    // where `(reason)` appears for SKIPPED / XFAIL / XPASS.
    const STATUS_TABLE: &[(&str, TestOutcome)] = &[
        ("PASSED", TestOutcome::Pass),
        ("XFAIL", TestOutcome::Pass),
        ("FAILED", TestOutcome::Fail),
        ("ERROR", TestOutcome::Fail),
        ("XPASS", TestOutcome::Fail),
        ("SKIPPED", TestOutcome::Skip),
    ];
    let is_ws_byte = |b: u8| matches!(b, b' ' | b'\t');
    let mut out = std::collections::HashMap::new();
    for line in stdout.lines() {
        if !line.contains("::") {
            continue;
        }
        // Strip trailing `[<digits>%]` progress suffix. Only strip
        // if the bracket content ends with `%]` so a parametrize
        // value like `test_p[42%]` doesn't get truncated (unlikely
        // but cheap to guard).
        let core = match line.rfind('[') {
            Some(bracket_idx) if line[bracket_idx..].ends_with("%]") => {
                line[..bracket_idx].trim_end()
            }
            _ => line.trim_end(),
        };
        // R8 codex P2: strip the BALANCED trailing `(reason)`
        // block. R7 used `rfind('(')` which only stripped the
        // INNERMOST paren, so a nested reason like
        // `SKIPPED (depends on FAILED API (ticket))` left
        // `... SKIPPED (depends on FAILED API` and `FAILED` still
        // won the rightmost-status scan. Scan backward from the
        // trailing `)` tracking paren depth; the OUTERMOST open
        // paren is where depth returns to 0. pytest's parametrize
        // uses `[...]` not `(...)`, so trailing parens are
        // always a reason block — never part of the test id.
        let core = if core.as_bytes().last() == Some(&b')') {
            let bytes = core.as_bytes();
            let mut depth: i32 = 0;
            let mut outer_open: Option<usize> = None;
            for (i, &b) in bytes.iter().enumerate().rev() {
                match b {
                    b')' => depth += 1,
                    b'(' => {
                        depth -= 1;
                        if depth == 0 {
                            outer_open = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            match outer_open {
                Some(i) => core[..i].trim_end(),
                None => core, // unbalanced — leave alone
            }
        } else {
            core
        };
        let bytes = core.as_bytes();
        // Scan all status keywords; track the RIGHTMOST
        // word-boundary-aligned match.
        let mut best: Option<(usize, TestOutcome)> = None;
        for (keyword, outcome) in STATUS_TABLE {
            for (idx, _) in core.match_indices(keyword) {
                let before_ok = idx == 0 || is_ws_byte(bytes[idx - 1]);
                let after = idx + keyword.len();
                let after_ok = after == core.len() || is_ws_byte(bytes[after]);
                if before_ok && after_ok && best.as_ref().map_or(true, |(i, _)| *i < idx) {
                    best = Some((idx, outcome.clone()));
                }
            }
        }
        if let Some((pos, outcome)) = best {
            let id = core[..pos].trim_end();
            if id.contains("::") {
                out.insert(id.to_string(), outcome);
            }
        }
    }
    out
}

/// Shell out to `pytest --collect-only -q` inside `repo_root` and
/// parse the emitted test ids. Failure modes:
///
/// - `ModuleNotFoundError` / `ImportError` on either stream (pytest
///   writes collection-time tracebacks to stdout) →
///   `PytestError::DependenciesUnresolved` (actionable — user needs
///   to `pip install` their package).
/// - Any other non-zero exit → `PytestError::Discovery` with both
///   pipes merged for forensic value.
///
/// `-q` output is one test id per line, followed by a summary line
/// that does NOT contain `::`; the filter is robust to that.
pub async fn discover_pytest_tests(repo_root: &Path) -> Result<TestUniverse, ImpactError> {
    // `--color=no` disables ANSI escape codes that would otherwise
    // wrap tokens and break substring-based signal detection. The
    // runner uses the same flag for symmetric reasons (R5 codex P2
    // sibling site).
    let out = Command::new("pytest")
        .arg("--collect-only")
        .arg("-q")
        .arg("--color=no")
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| ImpactError::Backend(Box::new(PytestError::Io(e))))?;
    if !out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        if pytest_output_signals_dependency_error(&stdout, &stderr) {
            return Err(ImpactError::Backend(Box::new(
                PytestError::DependenciesUnresolved(merge_pipes(&stdout, &stderr)),
            )));
        }
        return Err(ImpactError::Backend(Box::new(PytestError::Discovery(
            merge_pipes(&stdout, &stderr),
        ))));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let tests: Vec<TestId> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| l.contains("::"))
        .map(TestId::new)
        .collect();
    Ok(TestUniverse::from_tests(tests))
}

/// Concatenate stdout+stderr for forensic rendering. Needed because
/// pytest splits useful info across both pipes depending on phase
/// (collection errors → stdout; interpreter crash → stderr).
fn merge_pipes(stdout: &str, stderr: &str) -> String {
    if stderr.is_empty() {
        stdout.to_string()
    } else if stdout.is_empty() {
        stderr.to_string()
    } else {
        format!("{stdout}\n{stderr}")
    }
}

/// Live pytest runner. Guarded behind the `live-tools` feature for
/// its integration test because `pytest` is not a CI dependency.
///
/// v2.1 runner shape is **pragmatic**: `-q` output surfaces per-test
/// status as dots/F, which we do not parse. We map the overall exit
/// code across all selected tests (pass → every test Pass; fail →
/// every test Fail), and stash stdout+stderr in `detail` so forensic
/// rendering still shows the failing lines.
#[derive(Default)]
pub struct PytestRunner {
    /// Extra args appended after the test ids (e.g. `-x`, `--tb=long`).
    pub extra_args: Vec<String>,
}

#[async_trait]
impl TestRunner for PytestRunner {
    fn name(&self) -> &'static str {
        "pytest"
    }

    async fn run(&self, repo_root: &Path, plan: &TestPlan) -> Result<TestRunSummary, ImpactError> {
        if plan.is_empty() {
            return Ok(TestRunSummary::default());
        }
        // ARG_MAX caveat (R2 gemini MED, deferred): passing N test ids
        // as individual argv entries hits Linux's `ARG_MAX` (~2 MiB
        // typical, ~128 KiB hard-floor) at roughly 20k-40k tests,
        // given pytest ids are 50-100 bytes each. v2.1 plans are
        // bounded well under that — real heuristic emits ≤100 ids
        // per turn. v2.2 batching mitigation: chunk `plan.tests` into
        // groups of 500 and spawn a `pytest` per chunk, aggregating
        // `TestRunResult` vectors. Not shipped here to keep v2.1
        // one-invocation semantics; revisit when eval seeds grow past
        // 5k tests OR when a user hits the limit in the wild.
        // `-v` emits one status line per test so we can parse
        // per-test outcomes instead of collapsing every test to
        // the overall exit code (R3 gemini HIGH).
        let mut cmd = Command::new("pytest");
        // `--color=no` disables ANSI escape codes that would wrap
        // status tokens (`\x1b[32mPASSED\x1b[0m`) and make the
        // byte-search parser miss them, silently degrading every
        // outcome to Unknown. R5 codex P2.
        cmd.arg("-v")
            .arg("--no-header")
            .arg("--tb=short")
            .arg("--color=no");
        for t in &plan.tests {
            cmd.arg(t.as_str());
        }
        for a in &self.extra_args {
            cmd.arg(a);
        }
        let out = cmd
            .current_dir(repo_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| ImpactError::Backend(Box::new(PytestError::Io(e))))?;
        let stdout_text = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        // Check BOTH streams for dependency-level failures (R2
        // codex P1/P2 + R3 narrowing). The helper is now tight
        // enough to distinguish a user test body that references
        // `ImportError` from a real collection-phase import
        // failure, so a `pytest.raises(ImportError)` test no
        // longer misfires through this branch.
        if !out.status.success() && pytest_output_signals_dependency_error(&stdout_text, &stderr) {
            return Err(ImpactError::Backend(Box::new(
                PytestError::DependenciesUnresolved(merge_pipes(&stdout_text, &stderr)),
            )));
        }
        // R7 codex P2: pytest exit codes per the docs:
        //   0 = all tests passed
        //   1 = some tests failed
        //   2 = test execution interrupted (Ctrl-C)
        //   3 = internal error
        //   4 = pytest command-line usage error
        //   5 = no tests collected
        // Only 0 and 1 are meaningful per-test-outcome runs. Other
        // non-zero codes mean we can't trust the `-v` parser to
        // produce useful results — pytest may not have emitted any
        // status lines at all. Surface those as `Discovery` errors
        // so the caller knows the runner couldn't classify, rather
        // than silently reporting every test as `Unknown`.
        match out.status.code() {
            Some(0) | Some(1) => {} // normal pass/fail run, continue
            other => {
                return Err(ImpactError::Backend(Box::new(PytestError::Discovery(
                    format!(
                        "pytest exited with code {} (expected 0 or 1):\n{}",
                        other
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "<signal>".into()),
                        merge_pipes(&stdout_text, &stderr)
                    ),
                ))));
            }
        }
        // Per-test outcomes parsed from `-v` stdout. Tests that
        // don't appear in pytest's output surface as `Unknown` —
        // honest gap rather than guessed Pass/Fail.
        let outcomes = parse_pytest_verbose_outcomes(&stdout_text);
        let detail = {
            // Reuse `merge_pipes` (gemini R2 top-level summary) —
            // single source of truth for stdout+stderr combination.
            let mut text = merge_pipes(&stdout_text, &stderr);
            // `String::truncate` panics if the byte index is not a
            // char boundary. pytest output frequently contains
            // multi-byte UTF-8 (non-English paths, assertion diffs),
            // so walk back to the nearest boundary. UTF-8 codepoints
            // are ≤4 bytes, so this terminates in ≤3 iterations
            // (R1 gemini HIGH).
            if text.len() > MAX_DETAIL_BYTES {
                let mut cutoff = MAX_DETAIL_BYTES;
                while !text.is_char_boundary(cutoff) {
                    cutoff -= 1;
                }
                text.truncate(cutoff);
            }
            // `Arc::<str>::from(String)` borrows the heap buffer
            // and wraps it in an atomic refcount. Every per-test
            // `detail.clone()` below is then an Arc-inc, not a
            // 4 KiB allocation. R4 gemini MED on PR #24.
            Some(std::sync::Arc::<str>::from(text))
        };
        let results = plan
            .tests
            .iter()
            .map(|t| TestRunResult {
                id: t.clone(),
                outcome: outcomes
                    .get(t.as_str())
                    .cloned()
                    .unwrap_or(TestOutcome::Unknown),
                duration_ms: 0,
                detail: detail.clone(),
            })
            .collect();
        Ok(TestRunSummary { results })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_signal_detected_on_stdout_collection_error() {
        // R2 codex P1: pytest's collection reporter emits import
        // failures on stdout. The canonical wrapper wording is
        // "ImportError while importing test module '...'" — that's
        // the signal we tighten to after R3.
        let stdout = "============================= ERRORS =============================\n\
                      _____________________ ERROR collecting test_foo.py ______________________\n\
                      ImportError while importing test module '/tmp/x/test_foo.py'.\n\
                      ModuleNotFoundError: No module named 'mypackage'\n";
        let stderr = "";
        assert!(pytest_output_signals_dependency_error(stdout, stderr));
    }

    #[test]
    fn dependency_signal_detected_on_stderr_pytest_missing() {
        // pytest itself not installed surfaces at interpreter
        // bootstrap (before the terminal reporter starts) — on
        // stderr.
        let stdout = "";
        let stderr = "Traceback (most recent call last):\n\
                      ModuleNotFoundError: No module named 'pytest'\n";
        assert!(pytest_output_signals_dependency_error(stdout, stderr));
    }

    #[test]
    fn dependency_signal_false_on_unrelated_assertion_failure() {
        // A test that simply fails (AssertionError) is NOT a
        // dependency problem and must NOT trip the signal.
        let stdout = "FAILED test_foo.py::test_bar - AssertionError: assert 0 == 1\n";
        let stderr = "";
        assert!(!pytest_output_signals_dependency_error(stdout, stderr));
    }

    #[test]
    fn dependency_signal_false_on_test_body_that_references_importerror() {
        // R3 codex P2 regression guard: a failing test whose body
        // asserts on `ImportError` (e.g. `pytest.raises(ImportError)`
        // that DID NOT raise) produces output that contains the
        // bare string "ImportError" — but NOT the collection-phase
        // wrapper "ImportError while importing". The narrowed
        // helper must let this through as a regular test failure.
        let stdout = "test_pkg.py::test_import_raises FAILED\n\
                      \n\
                      ___________________ test_import_raises ___________________\n\
                      def test_import_raises():\n\
                      >       with pytest.raises(ImportError):\n\
                      E       Failed: DID NOT RAISE <class 'ImportError'>\n";
        let stderr = "";
        assert!(
            !pytest_output_signals_dependency_error(stdout, stderr),
            "bare `ImportError` in a test body must not misfire as dependency error"
        );
    }

    #[test]
    fn dependency_signal_false_on_test_body_that_quotes_import_wrapper_phrase() {
        // R6 codex P2: even the narrowed
        // `"ImportError while importing"` phrase can appear in
        // user test output if a test body asserts on that exact
        // string. Without the `"ERROR collecting"` anchor, this
        // would misfire as `DependenciesUnresolved` and convert
        // a real test failure into a misleading env error.
        let stdout = "tests/test_msgs.py::test_wrapper_msg FAILED\n\
                      E   AssertionError: expected substring 'ImportError while importing' in exc.args[0]\n\
                      E   assert 'ImportError while importing' in 'something else entirely'\n";
        let stderr = "";
        assert!(
            !pytest_output_signals_dependency_error(stdout, stderr),
            "test body asserting on `ImportError while importing` must not trip without the `ERROR collecting` anchor"
        );
    }

    #[test]
    fn dependency_signal_true_when_both_anchors_present_on_stdout() {
        // Confirm the R6 tightening didn't break the real-trigger
        // path: pytest's actual collection-error output carries
        // BOTH `ERROR collecting` AND `ImportError while
        // importing`, so the helper must still fire.
        let stdout = "============================= ERRORS =============================\n\
                      _____________________ ERROR collecting test_foo.py ______________________\n\
                      ImportError while importing test module '/tmp/x/test_foo.py'.\n\
                      ModuleNotFoundError: No module named 'mypackage'\n";
        let stderr = "";
        assert!(pytest_output_signals_dependency_error(stdout, stderr));
    }

    #[test]
    fn dependency_signal_false_on_test_body_referencing_pytest_missing_on_stdout() {
        // R5 codex P2: a test body that asserts on the string
        // "No module named 'pytest'" (e.g. validating exception
        // messaging for a sandboxed import) emits that literal on
        // stdout. The R3/R4 helper would misfire; the R5 narrow
        // restricts the missing-pytest signal to stderr only, so
        // this test-body stdout must NOT trip it.
        let stdout = "tests/test_msgs.py::test_pytest_missing_msg FAILED\n\
                      E   AssertionError: expected 'No module named \\'pytest\\'' in exc.args[0]\n";
        let stderr = "";
        assert!(
            !pytest_output_signals_dependency_error(stdout, stderr),
            "stdout reference to `No module named 'pytest'` must not trip the bootstrap signal"
        );
    }

    #[test]
    fn dependency_signal_false_on_test_body_that_references_modulenotfounderror() {
        // Sibling of the ImportError false-positive guard.
        let stdout = "test_pkg.py::test_missing_mod FAILED\n\
                      E   AssertionError: expected ModuleNotFoundError message\n\
                      E   assert 'No module named' in 'something else'\n";
        let stderr = "";
        assert!(
            !pytest_output_signals_dependency_error(stdout, stderr),
            "bare `ModuleNotFoundError` in a test body must not misfire"
        );
    }

    #[test]
    fn merge_pipes_handles_empty_either_side() {
        assert_eq!(merge_pipes("out", ""), "out");
        assert_eq!(merge_pipes("", "err"), "err");
        assert_eq!(merge_pipes("out", "err"), "out\nerr");
        assert_eq!(merge_pipes("", ""), "");
    }

    #[test]
    fn parse_verbose_outcomes_maps_passed_failed_skipped() {
        let stdout = "tests/test_sample.py::test_alpha PASSED                      [ 25%]\n\
                      tests/test_sample.py::test_beta FAILED                       [ 50%]\n\
                      tests/test_sample.py::test_gamma SKIPPED (some reason)       [ 75%]\n\
                      tests/test_sample.py::test_delta ERROR                       [100%]\n\
                      =================== 1 failed, 1 passed, 1 skipped, 1 error in 0.05s ===\n";
        let outcomes = parse_pytest_verbose_outcomes(stdout);
        assert_eq!(
            outcomes.get("tests/test_sample.py::test_alpha"),
            Some(&TestOutcome::Pass)
        );
        assert_eq!(
            outcomes.get("tests/test_sample.py::test_beta"),
            Some(&TestOutcome::Fail)
        );
        assert_eq!(
            outcomes.get("tests/test_sample.py::test_gamma"),
            Some(&TestOutcome::Skip)
        );
        assert_eq!(
            outcomes.get("tests/test_sample.py::test_delta"),
            Some(&TestOutcome::Fail),
            "pytest ERROR (collection/fixture failure) maps to Fail"
        );
    }

    #[test]
    fn parse_verbose_outcomes_handles_xfail_xpass() {
        let stdout = "tests/test_x.py::test_expected_fail XFAIL                    [ 50%]\n\
                      tests/test_x.py::test_unexpected_pass XPASS                  [100%]\n";
        let outcomes = parse_pytest_verbose_outcomes(stdout);
        assert_eq!(
            outcomes.get("tests/test_x.py::test_expected_fail"),
            Some(&TestOutcome::Pass),
            "XFAIL = expected-and-did failure = success for the user"
        );
        assert_eq!(
            outcomes.get("tests/test_x.py::test_unexpected_pass"),
            Some(&TestOutcome::Fail),
            "XPASS = expected-failure-that-passed = stale test, Fail"
        );
    }

    #[test]
    fn parse_verbose_outcomes_ignores_banner_and_summary_lines() {
        // Every line WITHOUT `::` must be skipped. The banner
        // lines and summary line contain other words but no
        // test id.
        let stdout = "============================= test session starts =============================\n\
                      collected 1 items\n\
                      \n\
                      tests/test_foo.py::test_bar PASSED                            [100%]\n\
                      \n\
                      ============================== 1 passed in 0.01s ==============================\n";
        let outcomes = parse_pytest_verbose_outcomes(stdout);
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes.contains_key("tests/test_foo.py::test_bar"));
    }

    #[test]
    fn parse_verbose_outcomes_missing_test_defaults_to_absent_not_pass() {
        // Tests that don't appear in the output surface as an
        // absent key — the caller uses `TestOutcome::Unknown` as
        // the default, which is the honest answer.
        let stdout = "tests/a.py::test_alpha PASSED  [100%]\n";
        let outcomes = parse_pytest_verbose_outcomes(stdout);
        assert!(!outcomes.contains_key("tests/a.py::test_beta"));
    }

    #[test]
    fn parse_verbose_outcomes_preserves_spaces_in_parametrize_brackets() {
        // R4 codex P1 regression guard: pytest parametrize can
        // embed whitespace in the node id — e.g. `[hello world]`
        // is a legitimate parametrize value that pytest emits
        // verbatim in its `-v` output. Naive `split_whitespace()`
        // would split the id itself into `...[hello` and `world]`,
        // pushing the status token to index 2 and dropping the line.
        let stdout =
            "tests/test_p.py::test_p[hello world] PASSED                          [ 25%]\n\
             tests/test_p.py::test_p[another case] FAILED                         [ 50%]\n\
             tests/test_p.py::test_p[just_one] SKIPPED                            [ 75%]\n\
             tests/test_p.py::test_p[multi word case here] XFAIL                  [100%]\n";
        let outcomes = parse_pytest_verbose_outcomes(stdout);
        assert_eq!(
            outcomes.get("tests/test_p.py::test_p[hello world]"),
            Some(&TestOutcome::Pass),
            "parametrize with single internal space must be preserved"
        );
        assert_eq!(
            outcomes.get("tests/test_p.py::test_p[another case]"),
            Some(&TestOutcome::Fail)
        );
        assert_eq!(
            outcomes.get("tests/test_p.py::test_p[just_one]"),
            Some(&TestOutcome::Skip),
            "no-space parametrize still works"
        );
        assert_eq!(
            outcomes.get("tests/test_p.py::test_p[multi word case here]"),
            Some(&TestOutcome::Pass),
            "3+ internal spaces must be preserved via byte-slice"
        );
    }

    #[test]
    fn parse_verbose_outcomes_handles_status_token_inside_parametrize_bracket() {
        // Pathological case: parametrize value that LOOKS like a
        // status keyword. The leading-space guard in the needle
        // means `[PASSED]` (no leading space) can't match, and
        // the boundary check rejects `[PASSEDX]` (no trailing
        // whitespace). Only the REAL status suffix matches.
        let stdout =
            "tests/test_p.py::test_p[PASSED] FAILED                                [100%]\n";
        let outcomes = parse_pytest_verbose_outcomes(stdout);
        assert_eq!(
            outcomes.get("tests/test_p.py::test_p[PASSED]"),
            Some(&TestOutcome::Fail),
            "status keyword embedded in bracket must not shadow the real status"
        );
    }

    #[test]
    fn parse_verbose_outcomes_error_boundary_not_matched_by_substring() {
        // Guard against `ERROR` matching a substring like `ERRORED`
        // that isn't pytest's status token.
        let stdout =
            "tests/test_q.py::test_q PASSED                                       [100%]\n\
             ============================== 1 ERRORED something else ==============================\n";
        let outcomes = parse_pytest_verbose_outcomes(stdout);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            outcomes.get("tests/test_q.py::test_q"),
            Some(&TestOutcome::Pass)
        );
    }

    #[test]
    fn parse_verbose_outcomes_strips_balanced_nested_reason_parens() {
        // R8 codex P2 regression guard: pytest reasons can contain
        // nested parentheses (e.g. someone references a Jira ticket
        // in parens within the reason). R7's `rfind('(')` would
        // strip only the INNERMOST `(ticket)`, leaving the rest of
        // the reason — including any embedded status keywords —
        // available for the rightmost-status scan to mis-match.
        // R8 walks paren depth backward to find the OUTERMOST
        // opening paren and strips from there.
        let stdout = "tests/test_x.py::test_a SKIPPED (depends on FAILED API (ticket))    [ 33%]\n\
             tests/test_x.py::test_b XFAIL (PASSED last release (see PR #42))    [ 66%]\n\
             tests/test_x.py::test_c SKIPPED (one (two (three)) four)            [100%]\n";
        let outcomes = parse_pytest_verbose_outcomes(stdout);
        assert_eq!(
            outcomes.get("tests/test_x.py::test_a"),
            Some(&TestOutcome::Skip),
            "R7 stripped innermost only and FAILED won — R8 must strip the full balanced block"
        );
        assert_eq!(
            outcomes.get("tests/test_x.py::test_b"),
            Some(&TestOutcome::Pass),
            "PASSED inside doubly-nested parens must not steal XFAIL"
        );
        assert_eq!(
            outcomes.get("tests/test_x.py::test_c"),
            Some(&TestOutcome::Skip),
            "triple-nested reason still parses correctly (no status keywords inside)"
        );
    }

    #[test]
    fn parse_verbose_outcomes_ignores_status_keyword_inside_skip_reason() {
        // R7 codex P2 regression guard: pytest emits a trailing
        // `(reason)` block after SKIPPED/XFAIL/XPASS. If the
        // reason text contains a status keyword (e.g.
        // `SKIPPED (depends on FAILED API)`), the R6 rightmost-
        // match algorithm would pick the keyword INSIDE the
        // reason instead of the real status. R7 strips the
        // trailing `(...)` block before scanning so the reason
        // text never contributes to the match.
        let stdout =
            "tests/test_x.py::test_a SKIPPED (depends on FAILED API)               [ 33%]\n\
             tests/test_x.py::test_b XFAIL (reason involves PASSED edge case)      [ 66%]\n\
             tests/test_x.py::test_c SKIPPED (no parens-wrapped status keyword)    [100%]\n";
        let outcomes = parse_pytest_verbose_outcomes(stdout);
        assert_eq!(
            outcomes.get("tests/test_x.py::test_a"),
            Some(&TestOutcome::Skip),
            "FAILED inside `(...)` reason must not steal the SKIPPED status"
        );
        assert_eq!(
            outcomes.get("tests/test_x.py::test_b"),
            Some(&TestOutcome::Pass),
            "PASSED inside `(...)` reason must not affect XFAIL outcome"
        );
        assert_eq!(
            outcomes.get("tests/test_x.py::test_c"),
            Some(&TestOutcome::Skip),
            "reason without status keyword still parses correctly"
        );
    }

    #[test]
    fn parse_verbose_outcomes_handles_status_keyword_embedded_in_parametrize_with_spaces() {
        // R6 codex P2 regression guard: pytest node ids can embed
        // status-keyword-shaped parametrize values like
        // `[ PASSED ]` (status-keyword with a leading space INSIDE
        // the bracket). The R4/R5 left-to-right `find(" PASSED")`
        // scan would match the bracket content first, boundary
        // check passes (trailing space before `]`), and the parser
        // returns the wrong id + wrong outcome. R6 parses from
        // the tail so the REAL status always wins.
        let stdout =
            "tests/test_p.py::test_p[ PASSED ] FAILED                              [ 50%]\n\
             tests/test_p.py::test_q[ FAILED ] PASSED                              [100%]\n";
        let outcomes = parse_pytest_verbose_outcomes(stdout);
        assert_eq!(
            outcomes.get("tests/test_p.py::test_p[ PASSED ]"),
            Some(&TestOutcome::Fail),
            "real trailing status FAILED must win over embedded ` PASSED `"
        );
        assert_eq!(
            outcomes.get("tests/test_p.py::test_q[ FAILED ]"),
            Some(&TestOutcome::Pass),
            "real trailing status PASSED must win over embedded ` FAILED `"
        );
    }

    #[test]
    fn parse_verbose_outcomes_trims_trailing_whitespace_from_test_id() {
        // R5 gemini HIGH regression guard: pytest's terminal
        // reporter may right-pad the test id with spaces to align
        // the status column across adjacent lines. A naive
        // `&line[..match_idx]` slice would capture those trailing
        // spaces into the map key, and subsequent lookups against
        // `plan.tests` (which hold un-padded TestId strings) would
        // miss, silently degrading every test to
        // `TestOutcome::Unknown`. The `trim()` on the sliced id
        // preserves internal whitespace (parametrize brackets)
        // while stripping alignment padding.
        let stdout =
            "tests/a.py::test_short        PASSED                                  [ 50%]\n\
             tests/a.py::test_longer_name  FAILED                                  [100%]\n";
        let outcomes = parse_pytest_verbose_outcomes(stdout);
        assert_eq!(
            outcomes.get("tests/a.py::test_short"),
            Some(&TestOutcome::Pass),
            "trailing whitespace must be trimmed so plan lookups succeed"
        );
        assert_eq!(
            outcomes.get("tests/a.py::test_longer_name"),
            Some(&TestOutcome::Fail)
        );
        // Invariant: no map key contains a trailing space.
        for key in outcomes.keys() {
            assert!(
                !key.ends_with(' '),
                "test id '{key}' has trailing whitespace — plan lookup would fail"
            );
        }
    }
}
