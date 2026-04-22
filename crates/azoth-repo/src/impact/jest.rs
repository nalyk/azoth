//! `JestImpact` — v2.1 JavaScript/TypeScript ecosystem selector.
//!
//! Shape mirrors [`super::pytest::PytestImpact`] so the TurnDriver can
//! swap selectors by language without reshape:
//!
//! - `with_universe(repo_root, universe)` — synthetic universe for
//!   integration tests, skips the `npx jest --listTests` shell-out.
//! - `discover(repo_root)` — production entry point; detects a jest
//!   config, rejects monorepo/workspaces shapes as
//!   `JestError::UnsupportedConfig`, then shells out to
//!   `npx jest --listTests` to enumerate the test universe.
//! - `detect(&Path)` — extension-free detector that returns
//!   `Ok(Some(kind_tag))` on single-project configs,
//!   `Err(UnsupportedConfig)` on monorepo/workspaces shapes,
//!   `Ok(None)` when no jest config is present.
//!
//! The selector heuristic is **direct filename-stem match** on the
//! absolute test file paths jest emits (confidence `1.0`). Symbol-graph
//! and co-edit widening are deferred to v2.2 — identical rationale to
//! PR-E.
//!
//! **Why `jest --json` for the runner**: pytest R3-R8 on PR #24 spent
//! 6 parser rewrites on `pytest -v` human-readable output. jest ships
//! a stable JSON reporter via `--json`, and `serde_json` is already a
//! workspace dep. Using it from day one avoids the text-parser trap.
//! See `feedback_parser_rewrite_count_is_a_signal.md` in auto-memory.

use std::collections::{HashMap, HashSet};
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
use super::runner::{TestOutcome, TestRunResult, TestRunSummary, TestRunner};

/// Selector-impl version. Bump on heuristic changes so replay can
/// detect plan drift without re-running the selector.
pub const JEST_IMPACT_VERSION: u32 = 1;

/// Forensic-detail truncation cap. Shared-floor with
/// `pytest::MAX_DETAIL_BYTES` by value but kept local so each runner
/// can diverge independently if its output shape demands a different
/// ceiling later.
const MAX_DETAIL_BYTES: usize = 4096;

/// Typed error surface for the jest backend. Boxed into
/// `ImpactError::Backend` at the selector boundary.
#[derive(Debug, Error)]
pub enum JestError {
    #[error(
        "jest not detected (no jest.config.{{js,ts,mjs,cjs}} / \
         package.json with `jest` section)"
    )]
    NotDetected,
    #[error(
        "jest monorepo config unsupported in v2.1 — `workspaces` or \
         `projects` arrays need per-project universes; revisit in v2.2"
    )]
    UnsupportedConfig,
    #[error("jest discovery failed: {0}")]
    Discovery(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// v2.1 JavaScript/TypeScript selector. Construction is explicit
/// (no `Default`) because every consumer must commit to
/// `with_universe` (tests) or `discover` (production).
pub struct JestImpact {
    repo_root: PathBuf,
    universe: TestUniverse,
}

impl JestImpact {
    /// Construct with an already-materialised universe. Integration
    /// tests feed synthetic universes; production uses `discover`.
    pub fn with_universe(repo_root: PathBuf, universe: TestUniverse) -> Self {
        Self {
            repo_root,
            universe,
        }
    }

    /// Production entry point: detect jest config, shell out to
    /// `npx jest --listTests`, build the universe.
    ///
    /// Error chain:
    /// - `NotDetected` — no jest config present
    /// - `UnsupportedConfig` — monorepo/workspaces shape detected
    /// - `Discovery(..)` — `npx jest` produced a non-zero exit
    pub async fn discover(repo_root: PathBuf) -> Result<Self, ImpactError> {
        match Self::detect(&repo_root) {
            Ok(Some(_)) => {
                let universe = discover_jest_tests(&repo_root).await?;
                Ok(Self {
                    repo_root,
                    universe,
                })
            }
            Ok(None) => Err(ImpactError::Backend(Box::new(JestError::NotDetected))),
            Err(e) => Err(ImpactError::Backend(Box::new(e))),
        }
    }

    /// Extension-free detector.
    ///
    /// - `Ok(Some(kind_tag))` — single-project jest config present;
    ///   `kind_tag` is stable routing input for future consumers.
    /// - `Ok(None)` — no jest config found.
    /// - `Err(UnsupportedConfig)` — monorepo shape detected
    ///   (`workspaces` or `projects` array in `package.json`).
    ///
    /// Sync `std::fs` I/O inside an `async fn` caller chain is
    /// intentional — tiny config files, read once at selector
    /// construction. Same rationale as `PytestImpact::detect`.
    ///
    /// Structured `serde_json` parse over `package.json` (not a
    /// substring probe) because a substring check would false-
    /// positive on any legitimate mention of `"jest"` /
    /// `"workspaces"` inside `description`, `scripts`, or
    /// dependency names. Top-level-key check is unambiguous.
    /// Malformed `package.json` surfaces as `Ok(None)` rather than
    /// an error — v2.1 isn't in the business of linting a user's
    /// repo, and downstream discovery will fail clearly via
    /// `Discovery(..)` when `npx jest` actually runs.
    pub fn detect(repo_root: &Path) -> Result<Option<&'static str>, JestError> {
        // Config files first — presence alone implies single-project.
        const CONFIG_FILES: &[&str] = &[
            "jest.config.js",
            "jest.config.ts",
            "jest.config.mjs",
            "jest.config.cjs",
        ];
        for name in CONFIG_FILES {
            if repo_root.join(name).exists() {
                return Ok(Some("jest_config_file"));
            }
        }
        // `read_to_string` returns `Err` on missing file, so no
        // preceding `exists()` stat is needed — also closes a TOCTOU
        // window (PR-E R1 gemini MED sibling).
        let pkg_text = match std::fs::read_to_string(repo_root.join("package.json")) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        let pkg: serde_json::Value = match serde_json::from_str(&pkg_text) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        // Monorepo guards FIRST — a repo with `jest` AND `workspaces`
        // is still UnsupportedConfig, not a single-project config.
        if pkg.get("workspaces").is_some() || pkg.get("projects").is_some() {
            return Err(JestError::UnsupportedConfig);
        }
        if pkg.get("jest").is_some() {
            return Ok(Some("package_json"));
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
impl ImpactSelector for JestImpact {
    fn name(&self) -> &'static str {
        "jest"
    }

    fn version(&self) -> u32 {
        JEST_IMPACT_VERSION
    }

    async fn select(&self, diff: &Diff, _contract: &Contract) -> Result<TestPlan, ImpactError> {
        if self.universe.is_empty() || diff.is_empty() {
            return Ok(TestPlan::empty(self.version()));
        }
        let mut plan = TestPlan::empty(self.version());
        // `HashSet<&str>` borrows from `self.universe` — same
        // zero-alloc pattern as `PytestImpact::select` (PR-E R2
        // gemini MED).
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

/// Shell out to `npx jest --listTests` inside `repo_root` and parse
/// the emitted absolute paths. Failure modes:
///
/// - `UnsupportedConfig` (monorepo/workspaces) — caught by
///   `detect(..)` before this function runs.
/// - Any non-zero exit → `JestError::Discovery` with both pipes merged.
///
/// `--listTests` prints one absolute path per line on stdout.
pub async fn discover_jest_tests(repo_root: &Path) -> Result<TestUniverse, ImpactError> {
    let out = Command::new("npx")
        .arg("jest")
        .arg("--listTests")
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| ImpactError::Backend(Box::new(JestError::Io(e))))?;
    if !out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        return Err(ImpactError::Backend(Box::new(JestError::Discovery(
            merge_pipes(&stdout, &stderr),
        ))));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let tests: Vec<TestId> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(TestId::new)
        .collect();
    Ok(TestUniverse::from_tests(tests))
}

/// Concatenate stdout+stderr for forensic rendering. Shared shape with
/// `pytest::merge_pipes` but kept local so the two backends can evolve
/// independently.
fn merge_pipes(stdout: &str, stderr: &str) -> String {
    if stderr.is_empty() {
        stdout.to_string()
    } else if stdout.is_empty() {
        stderr.to_string()
    } else {
        format!("{stdout}\n{stderr}")
    }
}

/// Subset of jest's `--json` output the runner consumes. Full schema
/// is much larger; we only deserialize what maps to `TestOutcome`.
///
/// jest emits camelCase field names. `#[serde(default)]` on the
/// envelope survives schema drift across jest versions — unknown
/// fields are ignored by serde by default, and the two fields we
/// consume are stable since jest v20.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JestRunOutput {
    #[serde(default)]
    test_results: Vec<JestFileResult>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JestFileResult {
    test_file_path: String,
    /// `"passed" | "failed" | "pending" | "skipped" | "todo"` —
    /// `pending`/`skipped`/`todo` all mean the file ran no real
    /// assertions, which maps cleanly to `TestOutcome::Skip`.
    status: String,
}

/// Map jest's file-level status to `TestOutcome`. Unrecognised
/// status strings surface as `Unknown` rather than guessing, matching
/// pytest's honest-gap policy.
fn status_to_outcome(status: &str) -> TestOutcome {
    match status {
        "passed" => TestOutcome::Pass,
        "failed" => TestOutcome::Fail,
        "pending" | "skipped" | "todo" => TestOutcome::Skip,
        _ => TestOutcome::Unknown,
    }
}

/// Parse jest's `--json` stdout into a `path → outcome` map. Returns
/// `None` when the stdout isn't valid JSON (jest failed before it
/// could emit the reporter output); caller surfaces that as a
/// `Discovery` error.
fn parse_jest_json(stdout: &str) -> Option<HashMap<String, TestOutcome>> {
    let parsed: JestRunOutput = serde_json::from_str(stdout).ok()?;
    let map = parsed
        .test_results
        .into_iter()
        .map(|fr| (fr.test_file_path, status_to_outcome(&fr.status)))
        .collect();
    Some(map)
}

/// Live jest runner. Guarded behind the `live-tools` feature for its
/// integration test because `npx jest` is not a CI dependency.
///
/// Consumes jest's `--json` reporter output — no text parsing, no
/// per-round edge-case fixes. If jest ever changes the JSON schema,
/// serde will fail deserialization cleanly and the runner surfaces
/// `Discovery(..)` rather than silently degrading to Unknown.
#[derive(Default)]
pub struct JestRunner {
    /// Extra args passed to jest BEFORE the internal flags. Per PR-E
    /// R11 gemini HIGH: user `extra_args` land first, then the
    /// internal flags the parser needs, so a user who supplies
    /// `--json=false` or a conflicting reporter flag can't silently
    /// break output parsing (jest/yargs follows last-flag-wins).
    pub extra_args: Vec<String>,
}

#[async_trait]
impl TestRunner for JestRunner {
    fn name(&self) -> &'static str {
        "jest"
    }

    async fn run(&self, repo_root: &Path, plan: &TestPlan) -> Result<TestRunSummary, ImpactError> {
        if plan.is_empty() {
            return Ok(TestRunSummary::default());
        }
        // ARG_MAX caveat (same shape as PR-E pytest runner): passing
        // N absolute paths as individual argv entries hits Linux
        // `ARG_MAX` (~128 KiB hard floor, ~2 MiB typical) at roughly
        // 500-1000 tests on macOS and ~10k on Linux. v2.1 heuristic
        // emits ≤100 ids per turn. Batching mitigation revisited in
        // v2.2 if eval seeds grow past 5k tests.
        let mut cmd = Command::new("npx");
        cmd.arg("jest");
        // PR-E R11 argv-precedence: user extra_args FIRST, internal
        // flags LAST, so jest/yargs' last-flag-wins semantics keeps
        // our mandatory `--json` / `--colors=false` / `--silent`
        // intact even if a user supplies `--json=false`.
        for a in &self.extra_args {
            cmd.arg(a);
        }
        cmd.arg("--json")
            // Disable ANSI color codes in jest's stderr/console
            // output so the forensic `detail` capture stays plain
            // text (jest's JSON reporter on stdout is unaffected).
            .arg("--colors=false")
            // Suppress `console.log` output from test bodies. Jest's
            // `--json` reporter emits only JSON on stdout, but
            // `console.*` calls from user tests still go to stdout
            // unless silenced — which would corrupt our
            // serde_json parse. `--silent` makes stdout pure JSON.
            .arg("--silent");
        // `--` separator: jest/yargs stops flag parsing here so a
        // test path starting with `-` (unusual but possible) isn't
        // interpreted as a flag. Mirrors PR-E R9.
        cmd.arg("--");
        for t in &plan.tests {
            cmd.arg(t.as_str());
        }
        let out = cmd
            .current_dir(repo_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| ImpactError::Backend(Box::new(JestError::Io(e))))?;
        let stdout_text = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr_text = String::from_utf8_lossy(&out.stderr).to_string();
        // Jest exit code is 0 when all tests pass, 1 when any test
        // fails. Other non-zero codes (typically 2 for CLI errors /
        // missing config / jest itself not installed) mean stdout
        // may not be valid JSON — surface as Discovery.
        let exit_code = out.status.code();
        let outcomes = match parse_jest_json(&stdout_text) {
            Some(m) => m,
            None => {
                return Err(ImpactError::Backend(Box::new(JestError::Discovery(
                    format!(
                        "jest exited with code {} and produced no parseable JSON on stdout:\n{}",
                        exit_code
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "<signal>".into()),
                        merge_pipes(&stdout_text, &stderr_text)
                    ),
                ))));
            }
        };
        // Detail buffer: merge stderr into the forensic tail. stdout
        // is jest's JSON (already parsed), so we intentionally carry
        // only stderr — the failing assertion output lives there in
        // `--silent` mode.
        let detail = {
            let mut text = stderr_text.clone();
            // `String::truncate` panics on non-char-boundary indices;
            // walk back to the nearest boundary. UTF-8 codepoints
            // are ≤4 bytes, so this terminates in ≤3 iterations
            // (PR-E R1 gemini HIGH sibling).
            if text.len() > MAX_DETAIL_BYTES {
                let mut cutoff = MAX_DETAIL_BYTES;
                while !text.is_char_boundary(cutoff) {
                    cutoff -= 1;
                }
                text.truncate(cutoff);
            }
            if text.is_empty() {
                None
            } else {
                // `Arc::<str>::from(String)` wraps the heap buffer in
                // atomic refcount. Per-test `detail.clone()` below is
                // an Arc-inc, not an allocation (PR-E R4 gemini MED).
                Some(Arc::<str>::from(text))
            }
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
    fn status_maps_cover_all_jest_status_strings() {
        assert_eq!(status_to_outcome("passed"), TestOutcome::Pass);
        assert_eq!(status_to_outcome("failed"), TestOutcome::Fail);
        assert_eq!(status_to_outcome("pending"), TestOutcome::Skip);
        assert_eq!(status_to_outcome("skipped"), TestOutcome::Skip);
        assert_eq!(status_to_outcome("todo"), TestOutcome::Skip);
        assert_eq!(status_to_outcome("???"), TestOutcome::Unknown);
    }

    #[test]
    fn parse_jest_json_maps_passed_failed_skipped() {
        // Minimal real jest `--json` envelope. Fields not consumed
        // (numTotalTests, perfStats, coverageMap, …) are left out so
        // the test locks only the public contract.
        let stdout = r#"{
            "success": false,
            "testResults": [
                { "testFilePath": "/abs/a.test.js", "status": "passed" },
                { "testFilePath": "/abs/b.test.js", "status": "failed" },
                { "testFilePath": "/abs/c.test.js", "status": "skipped" },
                { "testFilePath": "/abs/d.test.js", "status": "pending" },
                { "testFilePath": "/abs/e.test.js", "status": "todo" }
            ]
        }"#;
        let map = parse_jest_json(stdout).expect("json must parse");
        assert_eq!(map.get("/abs/a.test.js"), Some(&TestOutcome::Pass));
        assert_eq!(map.get("/abs/b.test.js"), Some(&TestOutcome::Fail));
        assert_eq!(map.get("/abs/c.test.js"), Some(&TestOutcome::Skip));
        assert_eq!(map.get("/abs/d.test.js"), Some(&TestOutcome::Skip));
        assert_eq!(map.get("/abs/e.test.js"), Some(&TestOutcome::Skip));
    }

    #[test]
    fn parse_jest_json_rejects_non_json_gracefully() {
        // When jest fails early (config error, missing module) it
        // prints a Node.js traceback on stdout instead of JSON. The
        // runner surfaces that as Discovery, not a silent Unknown.
        assert!(parse_jest_json("Error: Cannot find module 'jest'").is_none());
        assert!(parse_jest_json("").is_none());
        // Partially-valid JSON that misses the envelope shape should
        // also reject (serde_json tolerates missing fields via
        // `#[serde(default)]` but the outer string-not-object case
        // still fails).
        assert!(parse_jest_json("\"just a string\"").is_none());
    }

    #[test]
    fn parse_jest_json_tolerates_extra_unknown_fields() {
        // Forward-compat guard: if jest adds new fields in a future
        // release, serde must ignore them (default behaviour on
        // `#[derive(Deserialize)]` without `#[serde(deny_unknown_fields)]`).
        let stdout = r#"{
            "success": true,
            "testResults": [
                {
                    "testFilePath": "/abs/a.test.js",
                    "status": "passed",
                    "futureField": 42,
                    "anotherNewThing": { "nested": true }
                }
            ],
            "someTopLevelAddition": "whatever"
        }"#;
        let map = parse_jest_json(stdout).expect("unknown fields must not break parse");
        assert_eq!(map.get("/abs/a.test.js"), Some(&TestOutcome::Pass));
    }

    #[test]
    fn parse_jest_json_empty_results_array_is_ok() {
        // jest emits `{ "testResults": [] }` when no tests matched
        // the path filter. That's not an error — the selector just
        // matched zero universe entries.
        let stdout = r#"{ "success": true, "testResults": [] }"#;
        let map = parse_jest_json(stdout).expect("empty array must parse");
        assert!(map.is_empty());
    }

    #[test]
    fn merge_pipes_handles_empty_either_side() {
        assert_eq!(merge_pipes("out", ""), "out");
        assert_eq!(merge_pipes("", "err"), "err");
        assert_eq!(merge_pipes("out", "err"), "out\nerr");
        assert_eq!(merge_pipes("", ""), "");
    }
}
