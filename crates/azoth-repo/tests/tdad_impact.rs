//! Sprint 5 integration — `CargoTestImpact` against a real SQLite
//! co-edit graph.
//!
//! Builds a synthetic git history (`fast-import`) that makes
//! `src/foo.rs` and `src/bar.rs` frequent co-editors, runs the
//! Sprint 3 co-edit builder against it, then drives the Sprint 5
//! selector with a diff that touches only `src/foo.rs`. The plan
//! must include:
//!   1. The direct-filename match (test containing `foo` stem).
//!   2. The co-edit neighbour match (test containing `bar` stem).
//!
//! We also wrap the selector in `SelectorBackedImpactValidator` and
//! assert the pass/fail contract the TurnDriver depends on.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use azoth_core::event_store::migrations;
use azoth_core::impact::{Diff, DiffSource, ImpactSelector};
use azoth_core::retrieval::{CoEditConfig, GraphRetrieval};
use azoth_core::schemas::{Contract, ContractId, EffectBudget, Scope, ValidatorStatus};
use azoth_core::validators::{ImpactValidator, SelectorBackedImpactValidator};
use azoth_repo::history::{build, CoEditGraphRetrieval};
use azoth_repo::impact::{
    parse_porcelain_for_tests, CargoTestImpact, GitStatusDiffSource, TestUniverse,
};
use rusqlite::Connection;
use tempfile::TempDir;

fn contract_stub() -> Contract {
    Contract {
        id: ContractId::new(),
        goal: "tdad integration".into(),
        non_goals: Vec::new(),
        success_criteria: Vec::new(),
        scope: Scope::default(),
        effect_budget: EffectBudget::default(),
        notes: Vec::new(),
    }
}

fn fresh_mirror(repo_dir: &Path) -> Arc<Mutex<Connection>> {
    let db_path = repo_dir.join(".azoth").join("state.sqlite");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let mut conn = Connection::open(&db_path).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    migrations::run(&mut conn).unwrap();
    Arc::new(Mutex::new(conn))
}

fn run_git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_init(repo: &Path) {
    run_git(repo, &["init", "--quiet", "--initial-branch=main"]);
    run_git(repo, &["config", "user.email", "test@test.example"]);
    run_git(repo, &["config", "user.name", "test"]);
    run_git(repo, &["config", "commit.gpgsign", "false"]);
}

fn fast_import(repo: &Path, commits: &[(&[&str], u32)]) {
    let mut stream = String::new();
    for (i, (files, tick)) in commits.iter().enumerate() {
        let blob_mark = 10_000 + i;
        let commit_mark = 20_000 + i;
        let body = format!("c{i}\n");
        stream.push_str(&format!(
            "blob\nmark :{blob_mark}\ndata {}\n{body}",
            body.len()
        ));
        stream.push_str(&format!("commit refs/heads/main\nmark :{commit_mark}\n"));
        let ts = *tick as u64 * 10;
        stream.push_str(&format!(
            "author T <t@t> {ts} +0000\ncommitter T <t@t> {ts} +0000\n"
        ));
        let msg = format!("c{i}");
        stream.push_str(&format!("data {}\n{msg}\n", msg.len()));
        if i > 0 {
            stream.push_str(&format!("from :{}\n", commit_mark - 1));
        }
        for f in *files {
            stream.push_str(&format!("M 100644 :{blob_mark} {f}\n"));
        }
        stream.push('\n');
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git fast-import");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stream.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "fast-import failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn selector_folds_direct_match_and_real_co_edit_neighbour() {
    let td = TempDir::new().unwrap();
    let repo = td.path();
    git_init(repo);

    // Sprint 3 seeding: 6 commits, all pairing src/foo.rs + src/bar.rs
    // so `bar.rs` surfaces as foo.rs's top-1 co-edit neighbour.
    // Plus noise pairs so the graph isn't degenerate.
    let plan: Vec<(&[&str], u32)> = vec![
        (&["src/foo.rs", "src/bar.rs"], 1),
        (&["src/foo.rs", "src/bar.rs"], 2),
        (&["src/foo.rs", "src/bar.rs"], 3),
        (&["src/foo.rs", "src/bar.rs"], 4),
        (&["src/foo.rs", "src/bar.rs"], 5),
        (&["src/x.rs", "src/y.rs"], 6),
    ];

    fast_import(repo, &plan);

    let conn = fresh_mirror(repo);
    let stats = build(
        &conn,
        repo,
        CoEditConfig {
            window: 100,
            skip_large_commits: 50,
        },
    )
    .expect("co-edit build");
    assert!(stats.edges_written >= 1, "seed must produce ≥1 edge");

    let graph: Arc<dyn GraphRetrieval> = Arc::new(CoEditGraphRetrieval::new(conn));

    let universe = TestUniverse::from_tests([
        "my_crate::foo::tests::direct_hit",
        "my_crate::bar::tests::neighbour_hit",
        "my_crate::quux::tests::unrelated",
    ]);

    let selector =
        CargoTestImpact::with_universe(PathBuf::from(repo), universe).with_co_edit_graph(graph);

    // Diff touches only src/foo.rs. Plan must include:
    //   1. my_crate::foo::tests::direct_hit   (filename stem `foo`)
    //   2. my_crate::bar::tests::neighbour_hit (co-edit neighbour → stem `bar`)
    //   3. NOT my_crate::quux::tests::unrelated (stem `quux`, no edge)
    let diff = Diff::from_paths(["src/foo.rs"]);
    let plan = selector.select(&diff, &contract_stub()).await.unwrap();

    assert!(plan.is_well_formed(), "selector returned malformed plan");
    assert_eq!(
        plan.tests.len(),
        2,
        "expected 2 tests (direct + neighbour), got {:?}",
        plan.tests
    );

    let names: Vec<String> = plan.tests.iter().map(|t| t.0.clone()).collect();
    assert!(
        names.contains(&"my_crate::foo::tests::direct_hit".into()),
        "plan missing direct-filename match: {names:?}"
    );
    assert!(
        names.contains(&"my_crate::bar::tests::neighbour_hit".into()),
        "plan missing co-edit-neighbour match: {names:?}"
    );
    assert!(
        !names.contains(&"my_crate::quux::tests::unrelated".into()),
        "unrelated test leaked into plan: {names:?}"
    );

    // Confidence ordering: direct match first (1.0), neighbour
    // second (0.6). The selector walks widened_paths in order, so
    // direct always precedes neighbour.
    let (direct_idx, _) = plan
        .tests
        .iter()
        .enumerate()
        .find(|(_, t)| t.0 == "my_crate::foo::tests::direct_hit")
        .unwrap();
    let (neighbour_idx, _) = plan
        .tests
        .iter()
        .enumerate()
        .find(|(_, t)| t.0 == "my_crate::bar::tests::neighbour_hit")
        .unwrap();
    assert!(
        direct_idx < neighbour_idx,
        "direct match must precede neighbour match in plan order"
    );
    assert!(plan.confidence[direct_idx] > plan.confidence[neighbour_idx]);
}

#[tokio::test]
async fn impact_validator_wraps_selector_and_reports_pass_on_populated_plan() {
    let td = TempDir::new().unwrap();
    let repo = td.path();
    git_init(repo);
    fast_import(
        repo,
        &[
            (&["src/foo.rs", "src/bar.rs"], 1),
            (&["src/foo.rs", "src/bar.rs"], 2),
        ],
    );
    let conn = fresh_mirror(repo);
    let _ = build(
        &conn,
        repo,
        CoEditConfig {
            window: 50,
            skip_large_commits: 50,
        },
    )
    .unwrap();
    let graph: Arc<dyn GraphRetrieval> = Arc::new(CoEditGraphRetrieval::new(conn));

    let universe = TestUniverse::from_tests(["my_crate::foo::tests::a", "my_crate::bar::tests::b"]);
    let selector = Arc::new(
        CargoTestImpact::with_universe(PathBuf::from(repo), universe).with_co_edit_graph(graph),
    ) as Arc<dyn ImpactSelector>;

    let validator = SelectorBackedImpactValidator::new("impact:cargo_test", selector);
    assert_eq!(validator.selector_name(), "cargo_test");
    assert_eq!(
        validator.selector_version(),
        azoth_repo::impact::CARGO_TEST_IMPACT_VERSION
    );
    assert!(
        !validator.runs_tests(),
        "v2 plan-only — runner not wired yet"
    );

    let diff = Diff::from_paths(["src/foo.rs"]);
    let report = validator.validate(&contract_stub(), &diff).await;
    assert_eq!(report.status, ValidatorStatus::Pass);
    assert_eq!(report.name, "impact:cargo_test");
    let plan = report.plan.expect("validator carries plan through");
    assert!(!plan.is_empty(), "plan must include direct + neighbour");
    assert!(plan.is_well_formed());
}

#[tokio::test]
async fn selector_no_graph_falls_back_to_direct_match_only() {
    let td = TempDir::new().unwrap();
    let universe =
        TestUniverse::from_tests(["c::foo::tests::a", "c::foo::tests::b", "c::bar::tests::b"]);
    let selector = CargoTestImpact::with_universe(PathBuf::from(td.path()), universe);
    let diff = Diff::from_paths(["src/foo.rs"]);
    let plan = selector.select(&diff, &contract_stub()).await.unwrap();
    // Both foo-stem tests, no bar (no graph wired to bridge).
    let ids: Vec<String> = plan.tests.iter().map(|t| t.0.clone()).collect();
    assert!(ids.contains(&"c::foo::tests::a".into()));
    assert!(ids.contains(&"c::foo::tests::b".into()));
    assert!(!ids.contains(&"c::bar::tests::b".into()));
}

#[tokio::test]
async fn git_status_diff_source_reports_working_tree_changes() {
    let td = TempDir::new().unwrap();
    let repo = td.path().to_path_buf();
    git_init(&repo);
    // Seed one committed file so the repo has a HEAD.
    std::fs::write(repo.join("seed.rs"), "// seed\n").unwrap();
    run_git(&repo, &["add", "seed.rs"]);
    run_git(&repo, &["commit", "--quiet", "-m", "seed"]);

    // Dirty the work tree: modify seed.rs + add new.rs (untracked).
    std::fs::write(repo.join("seed.rs"), "// mutated\n").unwrap();
    std::fs::write(repo.join("new.rs"), "// fresh\n").unwrap();

    let src = GitStatusDiffSource::new(repo);
    let diff = src.diff().await.expect("git status succeeds");
    assert!(
        diff.changed_files.iter().any(|p| p == "seed.rs"),
        "modified file missing: {:?}",
        diff.changed_files
    );
    assert!(
        diff.changed_files.iter().any(|p| p == "new.rs"),
        "untracked file missing: {:?}",
        diff.changed_files
    );
    assert_eq!(src.name(), "git_status");
}

/// Regression guard: the porcelain parser must tolerate the exact
/// wire shape `git status --porcelain=v1` emits, including rename
/// `old -> new` lines and quoted paths with spaces. Kept here (not
/// in the unit module) so this test survives a refactor that moves
/// the parser's visibility.
#[test]
fn porcelain_parser_tolerates_rename_and_untracked() {
    let txt = " M src/a.rs\nR  src/old.rs -> src/new.rs\n?? src/fresh.rs\n";
    let d = parse_porcelain_for_tests(txt);
    assert_eq!(
        d.changed_files,
        vec!["src/a.rs", "src/new.rs", "src/fresh.rs"]
    );
}
