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
    pub fn detect(repo_root: &Path) -> Option<&'static str> {
        if repo_root.join("pytest.ini").exists() {
            return Some("pytest_ini");
        }
        if repo_root.join("pyproject.toml").exists() {
            if let Ok(s) = std::fs::read_to_string(repo_root.join("pyproject.toml")) {
                if s.contains("[tool.pytest.ini_options]") {
                    return Some("pyproject");
                }
            }
        }
        if repo_root.join("setup.cfg").exists() {
            if let Ok(s) = std::fs::read_to_string(repo_root.join("setup.cfg")) {
                if s.contains("[tool:pytest]") {
                    return Some("setup_cfg");
                }
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
        let mut seen: HashSet<String> = HashSet::new();
        for path in &diff.changed_files {
            let stem = Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if stem.is_empty() {
                continue;
            }
            for t in &self.universe.tests {
                if t.as_str().contains(stem) && seen.insert(t.as_str().to_string()) {
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

/// Shell out to `pytest --collect-only -q` inside `repo_root` and
/// parse the emitted test ids. Failure modes:
///
/// - `ModuleNotFoundError` / `ImportError` in stderr →
///   `PytestError::DependenciesUnresolved` (actionable — user needs
///   to `pip install` their package).
/// - Any other non-zero exit → `PytestError::Discovery` with stderr.
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
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        if stderr.contains("ModuleNotFoundError") || stderr.contains("ImportError") {
            return Err(ImpactError::Backend(Box::new(
                PytestError::DependenciesUnresolved(stderr),
            )));
        }
        return Err(ImpactError::Backend(Box::new(PytestError::Discovery(
            stderr,
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
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        if !out.status.success()
            && (stderr.contains("ModuleNotFoundError") || stderr.contains("ImportError"))
        {
            return Err(ImpactError::Backend(Box::new(
                PytestError::DependenciesUnresolved(stderr),
            )));
        }
        let all_pass = out.status.success();
        let detail = {
            let mut text = String::from_utf8_lossy(&out.stdout).to_string();
            text.push('\n');
            text.push_str(&stderr);
            if text.len() > 4096 {
                text.truncate(4096);
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
