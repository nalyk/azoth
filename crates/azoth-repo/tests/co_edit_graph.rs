//! Integration tests for Sprint 3 co-edit graph.
//!
//! Builds synthetic repos via `git fast-import` so the setup is
//! millisecond-fast even for the 500-commit budget case. fast-import
//! takes an explicit stream of blobs and commits, which lets us
//! deterministically choose what each commit "touches" without the
//! overhead of `git add`/`git commit` subprocess fork-exec per commit.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use azoth_core::event_store::migrations;
use azoth_core::retrieval::{CoEditConfig, GraphRetrieval};
use azoth_repo::history::{build, path_node, CoEditGraphRetrieval};
use rusqlite::Connection;
use tempfile::TempDir;

/// Open a brand-new mirror DB alongside `repo_dir` and return a
/// shared `Arc<Mutex<Connection>>` wired to it. The migrator runs
/// so m0004 (co_edit_edges) is available.
fn fresh_mirror(repo_dir: &Path) -> Arc<Mutex<Connection>> {
    let db_path = repo_dir.join(".azoth").join("state.sqlite");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let mut conn = Connection::open(&db_path).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.pragma_update(None, "synchronous", "NORMAL").unwrap();
    migrations::run(&mut conn).unwrap();
    Arc::new(Mutex::new(conn))
}

/// `git init` + user config in `repo_dir`. Shells out four times at
/// startup; not on the hot path of the tests themselves.
fn git_init(repo_dir: &Path) {
    run_git(repo_dir, &["init", "--quiet", "--initial-branch=main"]);
    run_git(repo_dir, &["config", "user.email", "test@test.example"]);
    run_git(repo_dir, &["config", "user.name", "test"]);
    run_git(repo_dir, &["config", "commit.gpgsign", "false"]);
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

/// Drive `git fast-import` with a precise stream of commits. Each
/// `(files, tick)` record becomes one commit whose content blob is
/// unique (so `--name-only` will list all its files), timestamped
/// at `tick * 10` seconds since the epoch. Returns when fast-import
/// has completed successfully.
fn fast_import(repo_dir: &Path, commits: &[(&[&str], u32)]) {
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
        .arg(repo_dir)
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
    // fast-import writes refs/heads/main but does not update the
    // work tree — for co-edit purposes we only need the commit
    // history, which `git log` walks without a checkout.
}

#[tokio::test]
async fn top_5_neighbors_match_expected_weights() {
    let td = TempDir::new().unwrap();
    let repo = td.path();
    git_init(repo);

    // Commit plan (50 total). `core.rs`'s top-5 neighbors by commit
    // count should be a > b > c > d > e, with z and distant files
    // absent.
    let mut plan: Vec<(&[&str], u32)> = Vec::new();
    let mut tick = 1u32;
    let mut push = |p: &mut Vec<(&[&str], u32)>, files: &'static [&'static str], times: usize| {
        for _ in 0..times {
            p.push((files, tick));
            tick += 1;
        }
    };
    push(&mut plan, &["core.rs", "a.rs"], 10);
    push(&mut plan, &["core.rs", "b.rs"], 8);
    push(&mut plan, &["core.rs", "c.rs"], 6);
    push(&mut plan, &["core.rs", "d.rs"], 4);
    push(&mut plan, &["core.rs", "e.rs"], 2);
    // Noise: unrelated pairs so the graph is not dominated by core.
    push(&mut plan, &["x.rs", "y.rs"], 10);
    push(&mut plan, &["a.rs", "z.rs"], 5);
    push(&mut plan, &["b.rs", "z.rs"], 5);
    assert_eq!(plan.len(), 50);

    fast_import(repo, &plan);

    let conn = fresh_mirror(repo);
    let cfg = CoEditConfig {
        window: 100,
        skip_large_commits: 50,
    };
    let stats = build(&conn, repo, cfg).expect("co-edit build");
    assert_eq!(stats.commits_walked, 50);
    assert_eq!(stats.commits_contributed, 50);
    assert_eq!(stats.commits_skipped_large, 0);
    // Distinct pairs: 5 core+{a..e} + 1 x+y + 2 {a,b}+z = 8 pairs.
    assert_eq!(stats.edges_written, 8);

    let graph = CoEditGraphRetrieval::new(conn);
    let hits = graph.neighbors(path_node("core.rs"), 1, 5).await.unwrap();

    let names: Vec<String> = hits
        .iter()
        .map(|(n, _)| n.0.strip_prefix("path:").unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        vec!["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"],
        "top-5 neighbors of core.rs in expected weight order"
    );

    // Weights: every commit here is 2-file (n-1 = 1), so each
    // commit contributes exactly 1.0 to its pair. Expect counts 10,
    // 8, 6, 4, 2.
    let expected_weights = [10.0, 8.0, 6.0, 4.0, 2.0];
    for (i, (_, edge)) in hits.iter().enumerate() {
        assert_eq!(edge.kind, "co_edit");
        assert!(
            (edge.weight - expected_weights[i]).abs() < 0.001,
            "weight {}: expected {}, got {}",
            i,
            expected_weights[i],
            edge.weight
        );
    }

    // `z.rs` is a neighbor of `a.rs` and `b.rs`, not `core.rs`. The
    // top-5 above must not include it (BFS depth=1).
    assert!(
        !names.iter().any(|n| n == "z.rs"),
        "z.rs must not surface at depth=1 for core.rs"
    );
}

#[tokio::test]
async fn unrelated_node_returns_empty() {
    let td = TempDir::new().unwrap();
    let repo = td.path();
    git_init(repo);
    // Single 2-file commit — nothing touching `lonely.rs`.
    fast_import(repo, &[(&["a.rs", "b.rs"], 1)]);

    let conn = fresh_mirror(repo);
    build(&conn, repo, CoEditConfig::default()).unwrap();
    let graph = CoEditGraphRetrieval::new(conn);
    let hits = graph.neighbors(path_node("lonely.rs"), 1, 5).await.unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn depth_2_bfs_reaches_second_hop() {
    let td = TempDir::new().unwrap();
    let repo = td.path();
    git_init(repo);
    // Chain: a—b, b—c, c—d. At depth=2 from `a`, `c` is reachable
    // but `d` is not.
    fast_import(
        repo,
        &[
            (&["a.rs", "b.rs"], 1),
            (&["b.rs", "c.rs"], 2),
            (&["c.rs", "d.rs"], 3),
        ],
    );

    let conn = fresh_mirror(repo);
    build(&conn, repo, CoEditConfig::default()).unwrap();
    let graph = CoEditGraphRetrieval::new(conn);

    let hop1 = graph.neighbors(path_node("a.rs"), 1, 10).await.unwrap();
    let names1: Vec<&str> = hop1
        .iter()
        .map(|(n, _)| n.0.strip_prefix("path:").unwrap())
        .collect();
    assert_eq!(names1, vec!["b.rs"]);

    let hop2 = graph.neighbors(path_node("a.rs"), 2, 10).await.unwrap();
    let mut names2: Vec<&str> = hop2
        .iter()
        .map(|(n, _)| n.0.strip_prefix("path:").unwrap())
        .collect();
    names2.sort();
    assert_eq!(names2, vec!["b.rs", "c.rs"]);

    let hop3 = graph.neighbors(path_node("a.rs"), 3, 10).await.unwrap();
    let mut names3: Vec<&str> = hop3
        .iter()
        .map(|(n, _)| n.0.strip_prefix("path:").unwrap())
        .collect();
    names3.sort();
    assert_eq!(names3, vec!["b.rs", "c.rs", "d.rs"]);
}

#[tokio::test]
async fn bfs_widest_path_not_last_hop() {
    // PR #7 review (codex P1). Graph:
    //   seed.rs — A.rs    ×1   (weight 1.0, weak link)
    //   A.rs    — C.rs    ×10  (weight 10.0, strong link)
    //   seed.rs — B.rs    ×5   (weight 5.0, direct)
    //
    // Pre-fix BFS reported C at 10.0 (only the last hop), which
    // ranked a weakly-connected transitive neighbor above the direct
    // neighbor B. Widest-path: C's strength = min(1.0, 10.0) = 1.0,
    // so at depth=2 the order is B(5.0) > A(1.0) ≈ C(1.0).
    let td = TempDir::new().unwrap();
    let repo = td.path();
    git_init(repo);

    let mut plan: Vec<(&[&str], u32)> = Vec::new();
    let mut tick = 1u32;
    let mut push = |p: &mut Vec<(&[&str], u32)>, files: &'static [&'static str], times: usize| {
        for _ in 0..times {
            p.push((files, tick));
            tick += 1;
        }
    };
    push(&mut plan, &["seed.rs", "A.rs"], 1);
    push(&mut plan, &["A.rs", "C.rs"], 10);
    push(&mut plan, &["seed.rs", "B.rs"], 5);
    fast_import(repo, &plan);

    let conn = fresh_mirror(repo);
    build(&conn, repo, CoEditConfig::default()).unwrap();
    let graph = CoEditGraphRetrieval::new(conn);

    let hits = graph.neighbors(path_node("seed.rs"), 2, 5).await.unwrap();
    let by_name: std::collections::HashMap<&str, f32> = hits
        .iter()
        .map(|(n, e)| (n.0.strip_prefix("path:").unwrap(), e.weight))
        .collect();

    assert!(
        (by_name["B.rs"] - 5.0).abs() < 1e-5,
        "B direct: {:?}",
        by_name
    );
    assert!(
        (by_name["A.rs"] - 1.0).abs() < 1e-5,
        "A direct: {:?}",
        by_name
    );
    assert!(
        (by_name["C.rs"] - 1.0).abs() < 1e-5,
        "C via A: weakest link dominates — pre-fix would have said 10.0. got {:?}",
        by_name
    );
    assert!(
        by_name["B.rs"] > by_name["C.rs"],
        "direct B must rank above indirect C: {by_name:?}"
    );
}

#[tokio::test]
async fn skip_large_commits_filters_wide_pr() {
    let td = TempDir::new().unwrap();
    let repo = td.path();
    git_init(repo);
    // One "squash PR" touching 10 files and one small commit.
    let wide: &[&str] = &[
        "f0.rs", "f1.rs", "f2.rs", "f3.rs", "f4.rs", "f5.rs", "f6.rs", "f7.rs", "f8.rs", "f9.rs",
    ];
    fast_import(repo, &[(wide, 1), (&["a.rs", "b.rs"], 2)]);

    let conn = fresh_mirror(repo);
    let cfg = CoEditConfig {
        window: 100,
        skip_large_commits: 5, // cuts the 10-file commit
    };
    let stats = build(&conn, repo, cfg).unwrap();
    assert_eq!(stats.commits_walked, 2);
    assert_eq!(stats.commits_skipped_large, 1);
    assert_eq!(stats.commits_contributed, 1);
    assert_eq!(stats.edges_written, 1); // only the a–b pair survived
}

#[tokio::test]
async fn rebuild_replaces_rather_than_accumulates() {
    let td = TempDir::new().unwrap();
    let repo = td.path();
    git_init(repo);
    fast_import(repo, &[(&["a.rs", "b.rs"], 1)]);

    let conn = fresh_mirror(repo);
    let s1 = build(&conn, repo, CoEditConfig::default()).unwrap();
    assert_eq!(s1.edges_written, 1);
    let s2 = build(&conn, repo, CoEditConfig::default()).unwrap();
    assert_eq!(s2.edges_written, 1, "re-build must not duplicate edges");

    let graph = CoEditGraphRetrieval::new(conn);
    let hits = graph.neighbors(path_node("a.rs"), 1, 5).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert!(
        (hits[0].1.weight - 1.0).abs() < 1e-6,
        "weight must not have doubled on rebuild: {}",
        hits[0].1.weight
    );
}

#[tokio::test]
#[ignore = "budget test — run with --ignored for ship-gate verification"]
async fn cold_build_on_500_commits_under_3s() {
    let td = TempDir::new().unwrap();
    let repo = td.path();
    git_init(repo);

    // 500 2-file commits across a rolling pool of 20 files.
    let pool: Vec<String> = (0..20).map(|i| format!("f{i}.rs")).collect();
    let refs: Vec<[&str; 2]> = (0..500)
        .map(|i| {
            let a = &pool[i % pool.len()];
            let b = &pool[(i * 7 + 3) % pool.len()];
            // fast_import expects &[&str] so we need stable storage;
            // build the tuple list below.
            [a.as_str(), b.as_str()]
        })
        .collect();
    let plan: Vec<(&[&str], u32)> = refs
        .iter()
        .enumerate()
        .filter(|(_, r)| r[0] != r[1])
        .map(|(i, r)| (r.as_slice(), i as u32 + 1))
        .collect();
    fast_import(repo, &plan);

    let conn = fresh_mirror(repo);
    let cfg = CoEditConfig {
        window: 500,
        skip_large_commits: 50,
    };
    let stats = build(&conn, repo, cfg).expect("co-edit build");
    assert!(
        stats.elapsed_ms <= 3_000,
        "500-commit cold build over budget: {} ms",
        stats.elapsed_ms
    );
    assert!(stats.commits_contributed > 400, "too many skips");
}
