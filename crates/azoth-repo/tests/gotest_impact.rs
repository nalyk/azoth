//! Integration tests for the v2.1 `GoTestImpact` selector. Pure — no
//! subprocess, no `go` required. The live `discover` path is
//! exercised in `gotest_runner.rs` behind the `live-tools` feature.

use azoth_core::impact::ImpactSelector;
use azoth_core::schemas::{Contract, ContractId, Diff, EffectBudget, Scope};
use azoth_repo::impact::{gotest::GoTestError, GoTestImpact, TestUniverse};
use tempfile::TempDir;

fn stub_contract() -> Contract {
    Contract {
        id: ContractId::new(),
        goal: "gotest impact".into(),
        non_goals: Vec::new(),
        success_criteria: Vec::new(),
        scope: Scope::default(),
        effect_budget: EffectBudget::default(),
        notes: Vec::new(),
    }
}

#[test]
fn detection_go_mod_present_returns_go_mod() {
    let td = TempDir::new().unwrap();
    std::fs::write(
        td.path().join("go.mod"),
        "module example.com/probe\n\ngo 1.21\n",
    )
    .unwrap();
    assert_eq!(GoTestImpact::detect(td.path()).unwrap(), Some("go_mod"));
}

#[test]
fn detection_no_go_mod_returns_none() {
    let td = TempDir::new().unwrap();
    assert!(GoTestImpact::detect(td.path()).unwrap().is_none());
}

#[test]
fn detection_go_work_flags_unsupported_even_with_go_mod() {
    // go.work wins over go.mod — Go's workspace mode supersedes
    // single-module resolution. Presence of go.work means multi-
    // module repo which v2.1 rejects.
    let td = TempDir::new().unwrap();
    std::fs::write(td.path().join("go.mod"), "module example.com/probe\n").unwrap();
    std::fs::write(
        td.path().join("go.work"),
        "go 1.21\n\nuse (\n    ./a\n    ./b\n)\n",
    )
    .unwrap();
    assert!(matches!(
        GoTestImpact::detect(td.path()),
        Err(GoTestError::UnsupportedConfig)
    ));
}

#[test]
fn detection_go_work_alone_also_flags_unsupported() {
    // Workspace without a root-level go.mod — still unsupported.
    let td = TempDir::new().unwrap();
    std::fs::write(
        td.path().join("go.work"),
        "go 1.21\n\nuse (\n    ./a\n    ./b\n)\n",
    )
    .unwrap();
    assert!(matches!(
        GoTestImpact::detect(td.path()),
        Err(GoTestError::UnsupportedConfig)
    ));
}

#[tokio::test]
async fn selector_direct_package_hit_on_parent_dir_match() {
    // Test ids are `<pkg_import>::<TestName>`. A change to
    // `pkg/auth/tokens.go` (parent dir name "auth") must select every
    // test under `example.com/m/pkg/auth` — Go's package granularity.
    let universe = TestUniverse::from_tests([
        "example.com/m/pkg/auth::TestRefresh",
        "example.com/m/pkg/auth::TestExpiry",
        "example.com/m/pkg/other::TestUnrelated",
    ]);
    let sel = GoTestImpact::with_universe(std::path::PathBuf::from("/repo"), universe);
    let plan = sel
        .select(&Diff::from_paths(["pkg/auth/tokens.go"]), &stub_contract())
        .await
        .unwrap();
    // Both auth tests selected; unrelated package skipped.
    assert_eq!(plan.tests.len(), 2, "expected both auth tests: {plan:?}");
    assert!(plan
        .tests
        .iter()
        .all(|t| t.as_str().starts_with("example.com/m/pkg/auth::")));
    assert!(plan.is_well_formed());
    assert!((plan.confidence[0] - 1.0).abs() < f32::EPSILON);
    assert!(plan.rationale[0].contains("auth"));
}

#[tokio::test]
async fn selector_empty_universe_returns_empty_plan() {
    let sel =
        GoTestImpact::with_universe(std::path::PathBuf::from("/tmp"), TestUniverse::default());
    let plan = sel
        .select(&Diff::from_paths(["pkg/foo/bar.go"]), &stub_contract())
        .await
        .unwrap();
    assert!(plan.is_empty());
    assert!(plan.is_well_formed());
}

#[tokio::test]
async fn selector_empty_diff_returns_empty_plan() {
    let universe = TestUniverse::from_tests(["example.com/pkg/foo::TestFoo"]);
    let sel = GoTestImpact::with_universe(std::path::PathBuf::from("/repo"), universe);
    let plan = sel.select(&Diff::empty(), &stub_contract()).await.unwrap();
    assert!(plan.is_empty());
}

#[tokio::test]
async fn selector_rejects_word_boundary_collision_auth_vs_author() {
    // The class bug sweep: stem `auth` (from `pkg/auth/...`) must NOT
    // select tests in `pkg/author/...`. `word_boundary_contains`
    // guards the hit because the char after `auth` in `author` is
    // alphanumeric.
    let universe = TestUniverse::from_tests([
        "example.com/m/pkg/author::TestAuthorship",
        "example.com/m/pkg/auth::TestLoginFlow",
    ]);
    let sel = GoTestImpact::with_universe(std::path::PathBuf::from("/repo"), universe);
    let plan = sel
        .select(&Diff::from_paths(["pkg/auth/tokens.go"]), &stub_contract())
        .await
        .unwrap();
    assert_eq!(plan.tests.len(), 1, "only auth, not author: {plan:?}");
    assert!(plan.tests[0].as_str().ends_with("::TestLoginFlow"));
}

#[tokio::test]
async fn selector_rejects_common_dir_name_collision_across_packages() {
    // R2 gemini HIGH on PR #26: R1 matched on just the last parent
    // dir component (`internal`), which over-selected in repos with
    // multiple `*/internal` packages. A change in
    // `pkg/auth/internal/foo.go` must NOT pull tests from an
    // unrelated `pkg/db/internal` package — those are two different
    // Go packages with different import paths. The R2 fix uses the
    // FULL relative parent path (`pkg/auth/internal`) so the match
    // narrows to the specific suffix.
    let universe = TestUniverse::from_tests([
        "example.com/m/pkg/auth/internal::TestAuthInternal",
        "example.com/m/pkg/db/internal::TestDbInternal",
        "example.com/m/pkg/api/internal::TestApiInternal",
    ]);
    let sel = GoTestImpact::with_universe(std::path::PathBuf::from("/repo"), universe);
    let plan = sel
        .select(
            &Diff::from_paths(["pkg/auth/internal/foo.go"]),
            &stub_contract(),
        )
        .await
        .unwrap();
    assert_eq!(
        plan.tests.len(),
        1,
        "only the auth/internal pkg; db/internal + api/internal must be skipped: {plan:?}"
    );
    assert!(plan.tests[0].as_str().ends_with("::TestAuthInternal"));
}

#[tokio::test]
async fn selector_dedupes_across_multiple_changed_files_in_same_pkg() {
    // Two changed files in the same package: plan must still include
    // each test only once (PR-E/F dedupe pattern).
    let universe = TestUniverse::from_tests(["example.com/m/pkg/auth::TestRefresh"]);
    let sel = GoTestImpact::with_universe(std::path::PathBuf::from("/repo"), universe);
    let plan = sel
        .select(
            &Diff::from_paths(["pkg/auth/tokens.go", "pkg/auth/sessions.go"]),
            &stub_contract(),
        )
        .await
        .unwrap();
    assert_eq!(plan.tests.len(), 1, "de-dupe failed: {plan:?}");
}

#[tokio::test]
async fn selector_skips_changed_files_without_named_parent() {
    // A bare filename at the repo root has parent="", which is
    // structurally empty — no package association possible. Must not
    // panic and must not pull unrelated tests.
    let universe = TestUniverse::from_tests(["example.com/m/pkg/auth::TestRefresh"]);
    let sel = GoTestImpact::with_universe(std::path::PathBuf::from("/repo"), universe);
    let plan = sel
        .select(&Diff::from_paths(["README.md"]), &stub_contract())
        .await
        .unwrap();
    assert!(
        plan.is_empty(),
        "empty-parent stem must short-circuit: {plan:?}"
    );
}

#[tokio::test]
async fn selector_ignores_malformed_universe_entries_missing_separator() {
    // Defensive — discovery always emits `pkg::name`, but if a
    // synthetic universe feeds an id without `::` the selector must
    // skip rather than crash. The malformed entry falls out of the
    // match (empty pkg_path → no match).
    let universe =
        TestUniverse::from_tests(["no-separator-entry", "example.com/m/pkg/auth::TestGood"]);
    let sel = GoTestImpact::with_universe(std::path::PathBuf::from("/repo"), universe);
    let plan = sel
        .select(&Diff::from_paths(["pkg/auth/tokens.go"]), &stub_contract())
        .await
        .unwrap();
    assert_eq!(
        plan.tests.len(),
        1,
        "malformed entry must be skipped: {plan:?}"
    );
    assert!(plan.tests[0].as_str().ends_with("::TestGood"));
}

#[tokio::test]
async fn selector_covers_all_single_file_diffs_on_ten_pair_fixture() {
    // Every `pkg/<name>/src.go` has a matching test in
    // `example.com/m/pkg/<name>::TestMain`. The plan asks for ≥80%
    // coverage on this fixture; the deterministic parent-dir
    // heuristic hits 100%.
    let pairs: [(&str, &str); 10] = [
        ("pkg/foo/src.go", "example.com/m/pkg/foo::TestMain"),
        ("pkg/bar/src.go", "example.com/m/pkg/bar::TestMain"),
        ("pkg/baz/src.go", "example.com/m/pkg/baz::TestMain"),
        ("pkg/qux/src.go", "example.com/m/pkg/qux::TestMain"),
        ("pkg/alpha/src.go", "example.com/m/pkg/alpha::TestMain"),
        ("pkg/beta/src.go", "example.com/m/pkg/beta::TestMain"),
        ("pkg/gamma/src.go", "example.com/m/pkg/gamma::TestMain"),
        ("pkg/delta/src.go", "example.com/m/pkg/delta::TestMain"),
        ("pkg/epsilon/src.go", "example.com/m/pkg/epsilon::TestMain"),
        ("pkg/zeta/src.go", "example.com/m/pkg/zeta::TestMain"),
    ];
    let universe = TestUniverse::from_tests(pairs.iter().map(|(_, t)| *t));
    let sel = GoTestImpact::with_universe(std::path::PathBuf::from("/repo"), universe);
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
        GoTestImpact::with_universe(std::path::PathBuf::from("/tmp"), TestUniverse::default());
    assert_eq!(sel.name(), "gotest");
    assert_eq!(sel.version(), azoth_repo::impact::GOTEST_IMPACT_VERSION);
}
