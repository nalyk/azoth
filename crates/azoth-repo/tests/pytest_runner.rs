//! Live `PytestRunner` agreement test. Gated behind the `live-tools`
//! feature so it is `#[ignore]`'d on machines without `pytest` on
//! `PATH`. Run explicitly with:
//!
//! ```bash
//! cargo test -p azoth-repo --features live-tools --test pytest_runner -- --ignored
//! ```

use azoth_core::schemas::{TestId, TestPlan};
use azoth_repo::impact::{PytestRunner, TestOutcome, TestRunner};
use tempfile::TempDir;

#[tokio::test]
#[cfg_attr(not(feature = "live-tools"), ignore = "requires pytest on PATH")]
async fn pytest_runner_agrees_with_pytest_on_mixed_pass_fail_fixture() {
    let td = TempDir::new().unwrap();
    std::fs::write(td.path().join("pytest.ini"), "[pytest]\n").unwrap();
    std::fs::write(
        td.path().join("test_sample.py"),
        "def test_pass():\n    assert True\n\n\
         def test_fail():\n    assert False\n\n\
         def test_also_pass():\n    assert 1 == 1\n",
    )
    .unwrap();

    let runner = PytestRunner::default();
    let plan = TestPlan {
        tests: vec![
            TestId::new("test_sample.py::test_pass"),
            TestId::new("test_sample.py::test_fail"),
            TestId::new("test_sample.py::test_also_pass"),
        ],
        rationale: vec!["".into(); 3],
        confidence: vec![1.0; 3],
        selector_version: 1,
    };

    let summary = runner.run(td.path(), &plan).await.unwrap();
    assert_eq!(summary.len(), 3);
    // v2.1 pragmatic shape — overall exit code maps to every test.
    // One failure sinks all, so every result must be Fail.
    assert!(summary
        .results
        .iter()
        .all(|r| r.outcome == TestOutcome::Fail));
    // Forensic detail should carry the failing assertion.
    assert!(summary.results[0]
        .detail
        .as_ref()
        .is_some_and(|d| d.contains("test_fail") || d.contains("AssertionError")));
}

#[tokio::test]
async fn pytest_runner_empty_plan_short_circuits() {
    // No subprocess — empty plan returns immediately. Safe to run
    // without the `live-tools` feature because `plan.is_empty()`
    // short-circuits before any `Command::spawn` call.
    let runner = PytestRunner::default();
    let plan = TestPlan::empty(1);
    let summary = runner
        .run(std::path::Path::new("/tmp"), &plan)
        .await
        .unwrap();
    assert!(summary.is_empty());
}
