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

/// Returns true when either pipe contains an `ImportError` /
/// `ModuleNotFoundError` marker. pytest's terminal reporter emits
/// collection-time import failures to **stdout**, not stderr
/// (stdout is the "test session output" channel; stderr is reserved
/// for interpreter-level failures). R2 codex P1/P2 — my R1 only
/// scanned stderr, so dependency errors slipped through as
/// `Discovery` with an empty message. Check both.
fn pytest_output_signals_dependency_error(stdout: &str, stderr: &str) -> bool {
    stdout.contains("ModuleNotFoundError")
        || stdout.contains("ImportError")
        || stderr.contains("ModuleNotFoundError")
        || stderr.contains("ImportError")
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
    let out = Command::new("pytest")
        .arg("--collect-only")
        .arg("-q")
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
        let mut cmd = Command::new("pytest");
        cmd.arg("-q").arg("--no-header").arg("--tb=short");
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
        // R2 codex P2: check BOTH streams. pytest emits collection
        // import errors on stdout, so a stderr-only scan loses the
        // actionable `DependenciesUnresolved` signal and marks
        // every planned test `Fail` instead.
        if !out.status.success() && pytest_output_signals_dependency_error(&stdout_text, &stderr) {
            return Err(ImpactError::Backend(Box::new(
                PytestError::DependenciesUnresolved(merge_pipes(&stdout_text, &stderr)),
            )));
        }
        let all_pass = out.status.success();
        let detail = {
            let mut text = stdout_text;
            text.push('\n');
            text.push_str(&stderr);
            // `String::truncate` panics if the byte index is not a
            // char boundary. pytest output frequently contains
            // multi-byte UTF-8 (non-English paths, assertion diffs),
            // so walk back to the nearest boundary. UTF-8 codepoints
            // are ≤4 bytes, so this terminates in ≤3 iterations
            // (R1 gemini HIGH).
            if text.len() > 4096 {
                let mut cutoff = 4096;
                while !text.is_char_boundary(cutoff) {
                    cutoff -= 1;
                }
                text.truncate(cutoff);
            }
            Some(text)
        };
        let results = plan
            .tests
            .iter()
            .map(|t| TestRunResult {
                id: t.clone(),
                outcome: if all_pass {
                    TestOutcome::Pass
                } else {
                    TestOutcome::Fail
                },
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
    fn dependency_signal_detected_on_stdout() {
        // R2 codex P1: pytest's collection reporter emits
        // ImportError on stdout. The R1 stderr-only check missed this.
        let stdout = "============================= ERRORS =============================\n\
                      _____________________ ERROR collecting test_foo.py ______________________\n\
                      ImportError while importing test module '.../test_foo.py'.\n\
                      ModuleNotFoundError: No module named 'mypackage'\n";
        let stderr = "";
        assert!(pytest_output_signals_dependency_error(stdout, stderr));
    }

    #[test]
    fn dependency_signal_detected_on_stderr() {
        // Interpreter-level failures do land on stderr. Both paths
        // must trigger the signal.
        let stdout = "";
        let stderr = "Traceback (most recent call last):\n\
                      ModuleNotFoundError: No module named 'pytest'\n";
        assert!(pytest_output_signals_dependency_error(stdout, stderr));
    }

    #[test]
    fn dependency_signal_false_on_unrelated_failure() {
        // A test that simply fails (AssertionError) is NOT a
        // dependency problem and must NOT trip the signal.
        let stdout = "FAILED test_foo.py::test_bar - AssertionError: assert 0 == 1\n";
        let stderr = "";
        assert!(!pytest_output_signals_dependency_error(stdout, stderr));
    }

    #[test]
    fn merge_pipes_handles_empty_either_side() {
        assert_eq!(merge_pipes("out", ""), "out");
        assert_eq!(merge_pipes("", "err"), "err");
        assert_eq!(merge_pipes("out", "err"), "out\nerr");
        assert_eq!(merge_pipes("", ""), "");
    }
}
