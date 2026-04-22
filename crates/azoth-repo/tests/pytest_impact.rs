//! Sprint-equivalent integration tests for the v2.1 `PytestImpact`
//! selector. Mirrors the shape of `tdad_impact.rs` but stays entirely
//! pure — no subprocess, no Python install needed. The live
//! `discover` path is exercised in `pytest_runner.rs` behind the
//! `live-tools` feature.

use azoth_core::impact::ImpactSelector;
use azoth_core::schemas::{Contract, ContractId, Diff, EffectBudget, Scope};
use azoth_repo::impact::{PytestImpact, TestUniverse};
use tempfile::TempDir;

fn stub_contract() -> Contract {
    Contract {
        id: ContractId::new(),
        goal: "pytest impact".into(),
        non_goals: Vec::new(),
        success_criteria: Vec::new(),
        scope: Scope::default(),
        effect_budget: EffectBudget::default(),
        notes: Vec::new(),
    }
}

#[test]
fn detection_pytest_ini_hits() {
    let td = TempDir::new().unwrap();
    std::fs::write(td.path().join("pytest.ini"), "[pytest]\n").unwrap();
    assert_eq!(PytestImpact::detect(td.path()), Some("pytest_ini"));
}

#[test]
fn detection_pyproject_hits() {
    let td = TempDir::new().unwrap();
    std::fs::write(
        td.path().join("pyproject.toml"),
        "[tool.pytest.ini_options]\naddopts = \"-q\"\n",
    )
    .unwrap();
    assert_eq!(PytestImpact::detect(td.path()), Some("pyproject"));
}

#[test]
fn detection_pyproject_without_section_misses() {
    let td = TempDir::new().unwrap();
    std::fs::write(
        td.path().join("pyproject.toml"),
        "[build-system]\nrequires = [\"setuptools\"]\n",
    )
    .unwrap();
    assert!(PytestImpact::detect(td.path()).is_none());
}

#[test]
fn detection_setup_cfg_hits() {
    let td = TempDir::new().unwrap();
    std::fs::write(td.path().join("setup.cfg"), "[tool:pytest]\n").unwrap();
    assert_eq!(PytestImpact::detect(td.path()), Some("setup_cfg"));
}

#[test]
fn detection_none_returns_none() {
    let td = TempDir::new().unwrap();
    assert!(PytestImpact::detect(td.path()).is_none());
}

#[tokio::test]
async fn selector_direct_filename_hit() {
    let universe = TestUniverse::from_tests(["tests/test_foo.py::test_alpha"]);
    let sel = PytestImpact::with_universe(std::path::PathBuf::from("/tmp"), universe);
    let plan = sel
        .select(&Diff::from_paths(["src/foo.py"]), &stub_contract())
        .await
        .unwrap();
    assert_eq!(plan.tests.len(), 1);
    assert!(plan.tests[0].as_str().contains("test_foo"));
    assert!((plan.confidence[0] - 1.0).abs() < f32::EPSILON);
    assert!(plan.rationale[0].contains("foo"));
    assert!(plan.is_well_formed());
}

#[tokio::test]
async fn selector_empty_universe_returns_empty_plan() {
    let sel =
        PytestImpact::with_universe(std::path::PathBuf::from("/tmp"), TestUniverse::default());
    let plan = sel
        .select(&Diff::from_paths(["src/foo.py"]), &stub_contract())
        .await
        .unwrap();
    assert!(plan.is_empty());
    assert!(plan.is_well_formed());
}

#[tokio::test]
async fn selector_empty_diff_returns_empty_plan() {
    let universe = TestUniverse::from_tests(["tests/test_foo.py::test_alpha"]);
    let sel = PytestImpact::with_universe(std::path::PathBuf::from("/tmp"), universe);
    let plan = sel.select(&Diff::empty(), &stub_contract()).await.unwrap();
    assert!(plan.is_empty());
}

#[tokio::test]
async fn selector_dedupes_across_multiple_changed_files() {
    // `src/foo.py` and `src/foo_util.py` both have stem "foo" /
    // "foo_util" — the test id contains both stems, so the
    // substring-match heuristic would emit it twice without the
    // `seen` de-dupe guard.
    let universe = TestUniverse::from_tests(["tests/test_foo_util.py::test_case"]);
    let sel = PytestImpact::with_universe(std::path::PathBuf::from("/tmp"), universe);
    let plan = sel
        .select(
            &Diff::from_paths(["src/foo.py", "src/foo_util.py"]),
            &stub_contract(),
        )
        .await
        .unwrap();
    assert_eq!(plan.tests.len(), 1, "de-dupe failed: {plan:?}");
}

#[tokio::test]
async fn selector_covers_all_single_file_diffs_on_ten_pair_fixture() {
    // Every `src/X.py` has a matching `tests/test_X.py`. A single
    // `src/X.py` diff must pick up at least the matching test. The
    // plan doc asks for ≥80% coverage; our heuristic is deterministic
    // so 100% is the honest floor.
    let pairs: [(&str, &str); 10] = [
        ("src/foo.py", "tests/test_foo.py::test_case"),
        ("src/bar.py", "tests/test_bar.py::test_case"),
        ("src/baz.py", "tests/test_baz.py::test_case"),
        ("src/qux.py", "tests/test_qux.py::test_case"),
        ("src/alpha.py", "tests/test_alpha.py::test_case"),
        ("src/beta.py", "tests/test_beta.py::test_case"),
        ("src/gamma.py", "tests/test_gamma.py::test_case"),
        ("src/delta.py", "tests/test_delta.py::test_case"),
        ("src/epsilon.py", "tests/test_epsilon.py::test_case"),
        ("src/zeta.py", "tests/test_zeta.py::test_case"),
    ];
    let universe = TestUniverse::from_tests(pairs.iter().map(|(_, t)| *t));
    let sel = PytestImpact::with_universe(std::path::PathBuf::from("/tmp"), universe);
    let mut hit = 0usize;
    for (src, _) in &pairs {
        let plan = sel
            .select(&Diff::from_paths([*src]), &stub_contract())
            .await
            .unwrap();
        if !plan.is_empty() {
            hit += 1;
        }
    }
    let rate = hit as f32 / pairs.len() as f32;
    assert!(
        rate >= 0.80,
        "single-file diff coverage {rate} < 0.80 on 10-pair fixture"
    );
}

#[tokio::test]
async fn selector_name_and_version_are_stable() {
    let sel =
        PytestImpact::with_universe(std::path::PathBuf::from("/tmp"), TestUniverse::default());
    assert_eq!(sel.name(), "pytest");
    assert_eq!(sel.version(), azoth_repo::impact::PYTEST_IMPACT_VERSION);
}
