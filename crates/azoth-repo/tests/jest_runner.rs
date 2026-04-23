//! Live `JestRunner` agreement test. Gated behind the `live-tools`
//! feature so it is `#[ignore]`'d on machines without `npx`/`jest` on
//! `PATH`. Run explicitly with:
//!
//! ```bash
//! cargo test -p azoth-repo --features live-tools --test jest_runner -- --ignored
//! ```
//!
//! The live path builds a minimal jest fixture inside a `TempDir` —
//! package.json with a `jest` section, two trivial test files (one
//! pass, one fail), and runs the full selector→runner pipeline.

use azoth_core::schemas::{TestId, TestPlan};
use azoth_repo::impact::{JestRunner, TestOutcome, TestRunner};
use tempfile::TempDir;

#[tokio::test]
#[cfg_attr(
    not(feature = "live-tools"),
    ignore = "requires npx/jest on PATH and a local `jest` install"
)]
async fn jest_runner_agrees_with_jest_on_mixed_pass_fail_fixture() {
    let td = TempDir::new().unwrap();
    // Minimal jest-enabled package.json. No `workspaces`, no
    // `projects`, so `JestImpact::detect` returns
    // `Ok(Some("package_json"))`.
    std::fs::write(
        td.path().join("package.json"),
        r#"{
            "name": "azoth-jest-test",
            "version": "0.0.0",
            "jest": { "testEnvironment": "node" }
        }"#,
    )
    .unwrap();
    let pass_path = td.path().join("pass.test.js");
    let fail_path = td.path().join("fail.test.js");
    std::fs::write(
        &pass_path,
        "test('passes', () => { expect(1).toBe(1); });\n",
    )
    .unwrap();
    std::fs::write(&fail_path, "test('fails', () => { expect(1).toBe(2); });\n").unwrap();

    let runner = JestRunner::default();
    let plan = TestPlan {
        tests: vec![
            TestId::new(pass_path.to_str().unwrap()),
            TestId::new(fail_path.to_str().unwrap()),
        ],
        rationale: vec!["".into(); 2],
        confidence: vec![1.0; 2],
        selector_version: 1,
    };

    let summary = runner.run(td.path(), &plan).await.unwrap();
    assert_eq!(summary.len(), 2);
    // Per-file granularity via `--json` reporter — no text parsing,
    // no per-round edge-case fixes. If jest ever changes its JSON
    // schema, serde fails clean and the runner surfaces Discovery.
    let by_path: std::collections::HashMap<&str, &TestOutcome> = summary
        .results
        .iter()
        .map(|r| (r.id.as_str(), &r.outcome))
        .collect();
    assert_eq!(
        by_path.get(pass_path.to_str().unwrap()),
        Some(&&TestOutcome::Pass),
        "pass.test.js must be reported as Pass"
    );
    assert_eq!(
        by_path.get(fail_path.to_str().unwrap()),
        Some(&&TestOutcome::Fail),
        "fail.test.js must be reported as Fail"
    );
    // Forensic detail should surface the failing expectation — jest
    // with `--silent` suppresses console.log but still emits test
    // failures on stderr.
    let detail = summary
        .results
        .iter()
        .find_map(|r| r.detail.clone())
        .expect("at least one result must carry forensic detail on a mixed pass/fail run");
    assert!(
        detail.contains("fails") || detail.contains("toBe") || detail.contains("Expected"),
        "detail must reference the failing expectation: {detail}"
    );
}

#[tokio::test]
async fn jest_runner_empty_plan_short_circuits() {
    // Safe to run without the `live-tools` feature because
    // `plan.is_empty()` short-circuits before any `Command::spawn`.
    let runner = JestRunner::default();
    let plan = TestPlan::empty(1);
    let summary = runner
        .run(std::path::Path::new("/tmp"), &plan)
        .await
        .unwrap();
    assert!(summary.is_empty());
}
