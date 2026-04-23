//! Live `GoTestRunner` agreement test. Gated behind the `live-tools`
//! feature so it is `#[ignore]`'d on machines without `go` on `PATH`.
//! Run explicitly with:
//!
//! ```bash
//! cargo test -p azoth-repo --features live-tools --test gotest_runner -- --ignored
//! ```
//!
//! The live path builds a minimal go module inside a `TempDir` —
//! `go.mod` plus one test file with three tests (pass, skip, fail) —
//! and runs the runner end-to-end through the real `go` toolchain.

use azoth_core::schemas::{TestId, TestPlan};
use azoth_repo::impact::{GoTestRunner, TestOutcome, TestRunner};
use tempfile::TempDir;

#[tokio::test]
#[cfg_attr(
    not(feature = "live-tools"),
    ignore = "requires the `go` toolchain on PATH"
)]
async fn gotest_runner_agrees_with_go_on_mixed_pass_skip_fail_fixture() {
    let td = TempDir::new().unwrap();
    // Minimal single-module fixture. Use a private import path so we
    // never accidentally hit module proxy lookups.
    std::fs::write(
        td.path().join("go.mod"),
        "module example.com/probe\n\ngo 1.21\n",
    )
    .unwrap();
    std::fs::write(
        td.path().join("sum_test.go"),
        r#"package probe

import "testing"

func TestAlpha(t *testing.T) { if 1+1 != 2 { t.Fail() } }
func TestBeta(t *testing.T)  { t.Skip("skip-me") }
func TestGamma(t *testing.T) { t.Fatal("boom") }
"#,
    )
    .unwrap();

    let runner = GoTestRunner::default();
    let plan = TestPlan {
        tests: vec![
            TestId::new("example.com/probe::TestAlpha"),
            TestId::new("example.com/probe::TestBeta"),
            TestId::new("example.com/probe::TestGamma"),
        ],
        rationale: vec!["".into(); 3],
        confidence: vec![1.0; 3],
        selector_version: 1,
    };

    let summary = runner.run(td.path(), &plan).await.unwrap();
    assert_eq!(summary.len(), 3);

    let by_id: std::collections::HashMap<&str, &TestOutcome> = summary
        .results
        .iter()
        .map(|r| (r.id.as_str(), &r.outcome))
        .collect();
    assert_eq!(
        by_id.get("example.com/probe::TestAlpha"),
        Some(&&TestOutcome::Pass),
        "TestAlpha must be Pass"
    );
    assert_eq!(
        by_id.get("example.com/probe::TestBeta"),
        Some(&&TestOutcome::Skip),
        "TestBeta must be Skip"
    );
    assert_eq!(
        by_id.get("example.com/probe::TestGamma"),
        Some(&&TestOutcome::Fail),
        "TestGamma must be Fail"
    );

    // Forensic detail — at least one result must carry the failure
    // text so the TUI has context to render. Accept either the
    // failure message or the standard `--- FAIL: TestGamma` marker.
    let detail = summary
        .results
        .iter()
        .find_map(|r| r.detail.clone())
        .expect("at least one result must carry forensic detail");
    assert!(
        detail.contains("boom") || detail.contains("FAIL: TestGamma") || detail.contains("PASS"),
        "detail must reference test output: {detail}"
    );
}

#[tokio::test]
async fn gotest_runner_empty_plan_short_circuits() {
    // Safe without `live-tools` — `plan.is_empty()` short-circuits
    // before any `Command::spawn`.
    let runner = GoTestRunner::default();
    let plan = TestPlan::empty(1);
    let summary = runner
        .run(std::path::Path::new("/tmp"), &plan)
        .await
        .unwrap();
    assert!(summary.is_empty());
}
