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

/// Regression guard for R1 gemini HIGH: the detail-capture buffer was
/// using `String::truncate(4096)` which panics when byte 4096 lands
/// mid-codepoint. This test constructs a string of exactly 4096
/// bytes + a multi-byte UTF-8 character straddling the boundary and
/// runs it through the same truncation loop the runner uses.
///
/// Before the fix the naive `truncate(4096)` would panic; after the
/// fix the loop walks back to the nearest char boundary.
#[test]
fn truncate_loop_is_char_boundary_safe_on_mid_codepoint() {
    // 4095 bytes of ASCII padding, then 'é' (2 bytes) — the 'é'
    // starts at byte 4095 and extends through byte 4096. A naive
    // `truncate(4096)` lands inside the 'é' and panics.
    let mut text = "a".repeat(4095);
    text.push('é');
    assert_eq!(text.len(), 4097);
    assert!(
        !text.is_char_boundary(4096),
        "fixture precondition: byte 4096 must NOT be a char boundary"
    );

    // Exact same loop shape the runner uses.
    if text.len() > 4096 {
        let mut cutoff = 4096;
        while !text.is_char_boundary(cutoff) {
            cutoff -= 1;
        }
        text.truncate(cutoff);
    }

    // Must have walked back to byte 4095 (start of 'é'), not 4096.
    assert_eq!(text.len(), 4095);
    assert!(text.is_ascii(), "truncated to all-ASCII prefix");
}
