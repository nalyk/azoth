//! Integration tests for the v2.1 `JestImpact` selector. Pure — no
//! subprocess, no `npx`/`node` required. The live `discover` path is
//! exercised in `jest_runner.rs` behind the `live-tools` feature.

use azoth_core::impact::ImpactSelector;
use azoth_core::schemas::{Contract, ContractId, Diff, EffectBudget, Scope};
use azoth_repo::impact::{jest::JestError, JestImpact, TestUniverse};
use tempfile::TempDir;

fn stub_contract() -> Contract {
    Contract {
        id: ContractId::new(),
        goal: "jest impact".into(),
        non_goals: Vec::new(),
        success_criteria: Vec::new(),
        scope: Scope::default(),
        effect_budget: EffectBudget::default(),
        notes: Vec::new(),
    }
}

#[test]
fn detection_jest_config_js_hits() {
    let td = TempDir::new().unwrap();
    std::fs::write(td.path().join("jest.config.js"), "module.exports = {};\n").unwrap();
    assert_eq!(
        JestImpact::detect(td.path()).unwrap(),
        Some("jest_config_file")
    );
}

#[test]
fn detection_jest_config_ts_hits() {
    let td = TempDir::new().unwrap();
    std::fs::write(
        td.path().join("jest.config.ts"),
        "export default { testEnvironment: 'node' };\n",
    )
    .unwrap();
    assert_eq!(
        JestImpact::detect(td.path()).unwrap(),
        Some("jest_config_file")
    );
}

#[test]
fn detection_jest_config_mjs_hits() {
    let td = TempDir::new().unwrap();
    std::fs::write(td.path().join("jest.config.mjs"), "export default {};\n").unwrap();
    assert_eq!(
        JestImpact::detect(td.path()).unwrap(),
        Some("jest_config_file")
    );
}

#[test]
fn detection_jest_config_cjs_hits() {
    let td = TempDir::new().unwrap();
    std::fs::write(td.path().join("jest.config.cjs"), "module.exports = {};\n").unwrap();
    assert_eq!(
        JestImpact::detect(td.path()).unwrap(),
        Some("jest_config_file")
    );
}

#[test]
fn detection_jest_config_json_hits() {
    // R3 gemini MED: jest.config.json is a first-class jest config file.
    let td = TempDir::new().unwrap();
    std::fs::write(
        td.path().join("jest.config.json"),
        r#"{"testEnvironment":"node"}"#,
    )
    .unwrap();
    assert_eq!(
        JestImpact::detect(td.path()).unwrap(),
        Some("jest_config_file")
    );
}

#[test]
fn detection_package_json_jest_section_hits() {
    let td = TempDir::new().unwrap();
    std::fs::write(
        td.path().join("package.json"),
        r#"{"name":"x","version":"0.0.0","jest":{}}"#,
    )
    .unwrap();
    assert_eq!(JestImpact::detect(td.path()).unwrap(), Some("package_json"));
}

#[test]
fn detection_package_json_without_jest_section_misses() {
    let td = TempDir::new().unwrap();
    std::fs::write(
        td.path().join("package.json"),
        r#"{"name":"x","version":"0.0.0","dependencies":{}}"#,
    )
    .unwrap();
    assert!(JestImpact::detect(td.path()).unwrap().is_none());
}

#[test]
fn detection_none_returns_none() {
    let td = TempDir::new().unwrap();
    assert!(JestImpact::detect(td.path()).unwrap().is_none());
}

#[test]
fn detection_description_mentioning_jest_does_not_false_positive() {
    // A substring probe on `"jest"` would misfire on any legitimate
    // mention of the word inside `description` or a `scripts`
    // command value. Structured JSON top-level-key check is
    // unambiguous — only `pkg.jest` / `pkg.workspaces` / `pkg.projects`
    // count.
    let td = TempDir::new().unwrap();
    std::fs::write(
        td.path().join("package.json"),
        r#"{
            "name": "jest-unrelated",
            "description": "a project that uses jest in docs but has no jest config",
            "scripts": { "test": "jest --watch" }
        }"#,
    )
    .unwrap();
    assert!(
        JestImpact::detect(td.path()).unwrap().is_none(),
        "substring `jest` in description/scripts must not false-positive detection"
    );
}

#[test]
fn detection_malformed_package_json_returns_none() {
    // A package.json that fails to parse must NOT crash detection —
    // treat it as "no jest config found" and let downstream discovery
    // surface the real error via `Discovery(..)` when `npx jest`
    // actually runs.
    let td = TempDir::new().unwrap();
    std::fs::write(td.path().join("package.json"), "{ this is not JSON").unwrap();
    assert!(JestImpact::detect(td.path()).unwrap().is_none());
}

#[test]
fn detection_workspaces_array_flags_unsupported() {
    // `workspaces` is the canonical yarn/npm monorepo marker. Jest's
    // multi-project execution model differs per-project, which breaks
    // the single-universe assumption of v2.1's selector.
    let td = TempDir::new().unwrap();
    std::fs::write(
        td.path().join("package.json"),
        r#"{"name":"x","workspaces":["packages/*"],"jest":{}}"#,
    )
    .unwrap();
    assert!(matches!(
        JestImpact::detect(td.path()),
        Err(JestError::UnsupportedConfig)
    ));
}

#[test]
fn detection_jest_projects_array_flags_unsupported() {
    // R1 codex P1: jest's multi-project shape lives under the `jest`
    // key as `jest.projects`, NOT at the package.json root. This is
    // the canonical jest monorepo config that v2.1 rejects.
    let td = TempDir::new().unwrap();
    std::fs::write(
        td.path().join("package.json"),
        r#"{"name":"x","jest":{"projects":["packages/a","packages/b"]}}"#,
    )
    .unwrap();
    assert!(matches!(
        JestImpact::detect(td.path()),
        Err(JestError::UnsupportedConfig)
    ));
}

#[test]
fn detection_workspaces_null_does_not_false_trigger() {
    // R3 gemini MED: `{"workspaces": null}` means "no monorepo"
    // (user left the key but disabled it). The pre-R3 `.is_some()`
    // check fired on any presence including null, producing a false
    // `UnsupportedConfig`. Null-safety via `is_some_and(!is_null)`
    // lets this fall through to the jest-section check.
    let td = TempDir::new().unwrap();
    std::fs::write(
        td.path().join("package.json"),
        r#"{"name":"x","workspaces":null,"jest":{}}"#,
    )
    .unwrap();
    assert!(matches!(
        JestImpact::detect(td.path()),
        Ok(Some("package_json"))
    ));
}

#[test]
fn detection_jest_key_null_returns_none() {
    // R4 gemini MED: `{"jest": null}` means the user explicitly
    // disabled jest at the package root — detect must return
    // `Ok(None)`, not `Ok(Some("package_json"))`. The null-safety
    // applies to the `jest` key itself, not only to `jest.projects`
    // and `workspaces`.
    let td = TempDir::new().unwrap();
    std::fs::write(
        td.path().join("package.json"),
        r#"{"name":"x","jest":null}"#,
    )
    .unwrap();
    assert!(matches!(JestImpact::detect(td.path()), Ok(None)));
}

#[test]
fn detection_jest_projects_null_does_not_false_trigger() {
    // R3 gemini MED sibling to the `workspaces: null` case. A
    // `{"jest": {"projects": null}}` means "no multi-project setup"
    // — must fall through as a normal single-project jest section.
    let td = TempDir::new().unwrap();
    std::fs::write(
        td.path().join("package.json"),
        r#"{"name":"x","jest":{"projects":null}}"#,
    )
    .unwrap();
    assert!(matches!(
        JestImpact::detect(td.path()),
        Ok(Some("package_json"))
    ));
}

#[test]
fn detection_top_level_projects_without_jest_is_ignored() {
    // R1 codex P1 sibling: a package.json with an unrelated top-level
    // `projects` field (some other tool's config — e.g. angular.json-
    // style workspace listings landing in package.json by accident)
    // must NOT be misread as a jest monorepo. No `jest` key ⇒ no jest
    // config ⇒ detect returns `Ok(None)`.
    let td = TempDir::new().unwrap();
    std::fs::write(
        td.path().join("package.json"),
        r#"{"name":"x","projects":["unrelated/a","unrelated/b"]}"#,
    )
    .unwrap();
    assert!(matches!(JestImpact::detect(td.path()), Ok(None)));
}

#[test]
fn detection_workspaces_precedence_over_jest_section() {
    // A package.json with BOTH `"jest"` and `"workspaces"` must fail
    // as UnsupportedConfig — the monorepo check runs first.
    let td = TempDir::new().unwrap();
    std::fs::write(
        td.path().join("package.json"),
        r#"{"name":"x","jest":{},"workspaces":["a","b"]}"#,
    )
    .unwrap();
    assert!(
        matches!(
            JestImpact::detect(td.path()),
            Err(JestError::UnsupportedConfig)
        ),
        "monorepo guard must win over single-project `jest` section"
    );
}

#[test]
fn detection_config_file_precedence_over_package_json_workspaces() {
    // A config file present AND `workspaces` in package.json: the
    // config-file presence is a stronger signal of single-project
    // intent, so it wins. This matches the v2.1 scope: we trust
    // explicit jest.config.* over package.json coexistence.
    let td = TempDir::new().unwrap();
    std::fs::write(td.path().join("jest.config.js"), "module.exports={};\n").unwrap();
    std::fs::write(
        td.path().join("package.json"),
        r#"{"name":"x","workspaces":["a"]}"#,
    )
    .unwrap();
    assert_eq!(
        JestImpact::detect(td.path()).unwrap(),
        Some("jest_config_file"),
        "explicit jest.config.* must win over package.json workspaces"
    );
}

#[tokio::test]
async fn selector_direct_filename_hit() {
    // Absolute paths — jest's `--listTests` emits them that way.
    let universe = TestUniverse::from_tests(["/repo/src/__tests__/foo.test.ts"]);
    let sel = JestImpact::with_universe(std::path::PathBuf::from("/repo"), universe);
    let plan = sel
        .select(&Diff::from_paths(["src/foo.ts"]), &stub_contract())
        .await
        .unwrap();
    assert_eq!(plan.tests.len(), 1);
    assert!(plan.tests[0].as_str().contains("foo.test.ts"));
    assert!((plan.confidence[0] - 1.0).abs() < f32::EPSILON);
    assert!(plan.rationale[0].contains("foo"));
    assert!(plan.is_well_formed());
}

#[tokio::test]
async fn selector_empty_universe_returns_empty_plan() {
    let sel = JestImpact::with_universe(std::path::PathBuf::from("/tmp"), TestUniverse::default());
    let plan = sel
        .select(&Diff::from_paths(["src/foo.ts"]), &stub_contract())
        .await
        .unwrap();
    assert!(plan.is_empty());
    assert!(plan.is_well_formed());
}

#[tokio::test]
async fn selector_empty_diff_returns_empty_plan() {
    let universe = TestUniverse::from_tests(["/repo/__tests__/foo.test.ts"]);
    let sel = JestImpact::with_universe(std::path::PathBuf::from("/repo"), universe);
    let plan = sel.select(&Diff::empty(), &stub_contract()).await.unwrap();
    assert!(plan.is_empty());
}

#[tokio::test]
async fn selector_dedupes_across_multiple_changed_files() {
    // Same de-dupe guard as pytest: two changed files whose stems
    // both match the same test path must pick it only once.
    let universe = TestUniverse::from_tests(["/repo/__tests__/foo_util.test.ts"]);
    let sel = JestImpact::with_universe(std::path::PathBuf::from("/repo"), universe);
    let plan = sel
        .select(
            &Diff::from_paths(["src/foo.ts", "src/foo_util.ts"]),
            &stub_contract(),
        )
        .await
        .unwrap();
    assert_eq!(plan.tests.len(), 1, "de-dupe failed: {plan:?}");
}

#[tokio::test]
async fn selector_covers_all_single_file_diffs_on_ten_pair_fixture() {
    // Every `src/X.ts` has a matching `__tests__/X.test.ts`. The
    // plan doc asks for ≥80% coverage on this fixture; our
    // deterministic heuristic hits 100%.
    let pairs: [(&str, &str); 10] = [
        ("src/foo.ts", "/repo/__tests__/foo.test.ts"),
        ("src/bar.ts", "/repo/__tests__/bar.test.ts"),
        ("src/baz.ts", "/repo/__tests__/baz.test.ts"),
        ("src/qux.ts", "/repo/__tests__/qux.test.ts"),
        ("src/alpha.ts", "/repo/__tests__/alpha.test.ts"),
        ("src/beta.ts", "/repo/__tests__/beta.test.ts"),
        ("src/gamma.ts", "/repo/__tests__/gamma.test.ts"),
        ("src/delta.ts", "/repo/__tests__/delta.test.ts"),
        ("src/epsilon.ts", "/repo/__tests__/epsilon.test.ts"),
        ("src/zeta.ts", "/repo/__tests__/zeta.test.ts"),
    ];
    let universe = TestUniverse::from_tests(pairs.iter().map(|(_, t)| *t));
    let sel = JestImpact::with_universe(std::path::PathBuf::from("/repo"), universe);
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
    let sel = JestImpact::with_universe(std::path::PathBuf::from("/tmp"), TestUniverse::default());
    assert_eq!(sel.name(), "jest");
    assert_eq!(sel.version(), azoth_repo::impact::JEST_IMPACT_VERSION);
}

#[tokio::test]
async fn selector_rejects_prefix_substring_author_vs_auth() {
    // R3 gemini MED: naive `contains("auth")` on `author.test.ts`
    // false-positively pulls unrelated `author` tests whenever a file
    // named `auth.ts` changes. Word-boundary guard must reject this.
    let universe = TestUniverse::from_tests(["/repo/src/__tests__/author.test.ts"].iter().copied());
    let sel = JestImpact::with_universe(std::path::PathBuf::from("/repo"), universe);
    let plan = sel
        .select(&Diff::from_paths(["src/auth.ts"]), &stub_contract())
        .await
        .unwrap();
    assert!(
        plan.is_empty(),
        "author.test.ts must not be selected by an auth.ts change: {plan:?}"
    );
}

#[tokio::test]
async fn selector_accepts_legitimate_auth_match_despite_word_boundary() {
    // Symmetric guard: word-boundary must NOT reject the canonical
    // `auth.ts` → `auth.test.ts` case it was designed to accept.
    let universe = TestUniverse::from_tests(["/repo/src/__tests__/auth.test.ts"].iter().copied());
    let sel = JestImpact::with_universe(std::path::PathBuf::from("/repo"), universe);
    let plan = sel
        .select(&Diff::from_paths(["src/auth.ts"]), &stub_contract())
        .await
        .unwrap();
    assert_eq!(plan.tests.len(), 1, "legitimate auth match: {plan:?}");
}
