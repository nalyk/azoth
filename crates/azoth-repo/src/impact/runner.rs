//! Shared runner surface for per-ecosystem test execution.
//!
//! `ImpactSelector` decides **which** tests to run; `TestRunner`
//! actually **runs** them. The split matches the two-phase shape the
//! TurnDriver uses in the validate phase: first select (may be cached
//! across retries), then run (fresh every retry).
//!
//! Each per-ecosystem module (pytest / jest / go test) ships a
//! concrete `TestRunner` alongside its `ImpactSelector`. The runner
//! trait is intentionally minimal — discovery, config detection, and
//! dependency-probing stay in the selector.

use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;

use azoth_core::impact::ImpactError;
use azoth_core::schemas::{TestId, TestPlan};

/// Outcome of running a single test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestOutcome {
    Pass,
    Fail,
    Skip,
    Unknown,
}

/// Per-test result emitted by a `TestRunner::run`. The rationale /
/// confidence from the upstream `TestPlan` are intentionally not
/// threaded here — callers zip the `TestPlan` with the summary when
/// they need provenance.
#[derive(Debug, Clone)]
pub struct TestRunResult {
    pub id: TestId,
    pub outcome: TestOutcome,
    pub duration_ms: u64,
    /// Captured stderr/stdout snippet (truncated to 4 KiB) for
    /// forensic rendering. `None` when the runner has no useful
    /// tail.
    ///
    /// `Arc<str>` (not `String`) because every `TestRunResult` in
    /// a single `run()` carries the SAME captured-output buffer
    /// — a plan with 100 tests × 4 KiB would clone 400 KiB of
    /// immutable text otherwise. `Arc<str>` makes per-result
    /// cloning atomic-increment-only. R4 gemini MED on PR #24.
    pub detail: Option<Arc<str>>,
}

/// Aggregate outcome of one `TestRunner::run` invocation.
#[derive(Debug, Clone, Default)]
pub struct TestRunSummary {
    pub results: Vec<TestRunResult>,
}

impl TestRunSummary {
    pub fn is_empty(&self) -> bool {
        self.results.is_empty()
    }

    pub fn len(&self) -> usize {
        self.results.len()
    }
}

#[async_trait]
pub trait TestRunner: Send + Sync {
    /// Human-readable runner name. Persisted alongside test-run
    /// events so forensic replay can distinguish pytest/jest/go-test
    /// runners without parsing test-id shape.
    fn name(&self) -> &'static str;

    /// `repo_root` is the working directory the runner shells into;
    /// `plan.tests` enumerates which tests to execute. The runner
    /// decides batching strategy — e.g. pytest takes many test ids
    /// in one invocation, jest is per-project.
    async fn run(&self, repo_root: &Path, plan: &TestPlan) -> Result<TestRunSummary, ImpactError>;
}
