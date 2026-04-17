//! Sprint 2 — tree-sitter symbol extraction end-to-end.
//!
//! Seeds a small synthetic Rust project into a tempdir, runs the Sprint 1
//! `RepoIndexer` (whose Phase 4 now triggers symbol extraction), and
//! exercises the `SqliteSymbolIndex` reader directly.
//!
//! Covers: kind classification for every `SymbolKind` variant we
//! promise to extract, parent-link integrity (method → impl,
//! variant → enum), line-range accuracy, `enclosing(path, line)`
//! semantics, and non-Rust files being ignored by the extractor.

use std::path::Path;
use std::sync::{Arc, Mutex};

use azoth_core::retrieval::{SymbolKind, SymbolRetrieval};
use azoth_repo::{RepoIndexer, SqliteSymbolIndex};
use tempfile::TempDir;

fn seed(root: &Path) {
    // `.ignore` (not `.gitignore`) is respected inside non-git tempdirs.
    std::fs::write(root.join(".ignore"), "ignored.log\n").unwrap();

    std::fs::write(
        root.join("lib.rs"),
        concat!(
            "pub fn top_level_fn() {}\n",
            "pub struct Foo { pub x: u32 }\n",
            "pub enum State { Ready, Done(u8) }\n",
            "pub trait Greet { fn hi(&self); }\n",
            "pub const PI: f32 = 3.14;\n",
            "pub mod sub { pub const K: u32 = 1; }\n",
        ),
    )
    .unwrap();

    std::fs::write(
        root.join("impls.rs"),
        concat!(
            "struct Target;\n",
            "impl Target {\n",
            "    pub fn one(&self) {}\n",
            "    pub fn two(&self) {}\n",
            "}\n",
        ),
    )
    .unwrap();

    // Non-Rust content must not produce symbols.
    std::fs::write(
        root.join("README.md"),
        "# title\n\nprose with keywords like Foo and State, no symbols here.\n",
    )
    .unwrap();
}

async fn seeded() -> (TempDir, SqliteSymbolIndex) {
    let td = TempDir::new().unwrap();
    let repo = td.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    seed(&repo);

    let db = td.path().join("state.sqlite");
    let indexer = RepoIndexer::open(&db, &repo).unwrap();
    let stats = indexer.reindex_incremental().await.unwrap();
    assert!(
        stats.symbols_extracted > 0,
        "Phase 4 hook must have extracted some symbols; got {stats:?}"
    );
    // Re-open a fresh connection in read mode to prove multi-connection
    // sharing still works with WAL (the persisted mode from Sprint 1).
    let sym_conn = rusqlite::Connection::open(&db).unwrap();
    let idx = SqliteSymbolIndex::new(Arc::new(Mutex::new(sym_conn)));
    (td, idx)
}

#[tokio::test]
async fn every_kind_roundtrips_through_extractor_and_index() {
    let (_td, idx) = seeded().await;

    // Function: top_level_fn
    let fns = idx.by_name("top_level_fn", 10).await.unwrap();
    assert!(fns.iter().any(|s| s.kind == SymbolKind::Function));

    // Struct: Foo
    let foo = idx.by_name("Foo", 10).await.unwrap();
    assert!(foo.iter().any(|s| s.kind == SymbolKind::Struct));

    // Enum + EnumVariants: State, Ready, Done
    let state = idx.by_name("State", 10).await.unwrap();
    assert!(state.iter().any(|s| s.kind == SymbolKind::Enum));
    let ready = idx.by_name("Ready", 10).await.unwrap();
    assert_eq!(ready.len(), 1, "exactly one Ready variant");
    assert_eq!(ready[0].kind, SymbolKind::EnumVariant);
    assert!(
        ready[0].parent_id.is_some(),
        "variant must link to its enum parent"
    );
    let done = idx.by_name("Done", 10).await.unwrap();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].kind, SymbolKind::EnumVariant);
    assert!(done[0].parent_id.is_some());

    // Trait: Greet
    let greet = idx.by_name("Greet", 10).await.unwrap();
    assert!(greet.iter().any(|s| s.kind == SymbolKind::Trait));

    // Const: PI, K
    let pi = idx.by_name("PI", 10).await.unwrap();
    assert!(pi.iter().any(|s| s.kind == SymbolKind::Const));
    let k = idx.by_name("K", 10).await.unwrap();
    assert_eq!(k.len(), 1);
    assert_eq!(k[0].kind, SymbolKind::Const);
    assert!(k[0].parent_id.is_some(), "const K must link to mod sub");

    // Module: sub
    let sub = idx.by_name("sub", 10).await.unwrap();
    assert!(sub.iter().any(|s| s.kind == SymbolKind::Module));

    // Impl: `impl Target` stores Target as the name.
    let target_impl = idx.by_name("Target", 10).await.unwrap();
    // One struct_item Target + one impl_item named "Target".
    assert!(target_impl.iter().any(|s| s.kind == SymbolKind::Struct));
    assert!(target_impl.iter().any(|s| s.kind == SymbolKind::Impl));

    // Method one(&self) — parent_id must point to impl Target.
    let one = idx.by_name("one", 10).await.unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].kind, SymbolKind::Function);
    let parent = one[0].parent_id.expect("method must have a parent_id");
    let parent_row = target_impl
        .iter()
        .find(|s| s.id == parent)
        .expect("parent_id must resolve to a row");
    assert_eq!(parent_row.kind, SymbolKind::Impl);
}

#[tokio::test]
async fn enclosing_resolves_method_span_inside_impl() {
    let (_td, idx) = seeded().await;
    // `impls.rs` is:
    //   1: struct Target;
    //   2: impl Target {
    //   3:     pub fn one(&self) {}
    //   4:     pub fn two(&self) {}
    //   5: }
    // Line 3 must return the function `one` (smallest range),
    // not the outer impl.
    let at_line_3 = idx
        .enclosing("impls.rs", 3)
        .await
        .unwrap()
        .expect("line 3 must be enclosed");
    assert_eq!(at_line_3.name, "one");
    assert_eq!(at_line_3.kind, SymbolKind::Function);

    // A line inside the impl but outside any method resolves to the
    // impl itself (range 2..=5).
    let at_line_5 = idx
        .enclosing("impls.rs", 5)
        .await
        .unwrap()
        .expect("line 5 must be enclosed");
    assert_eq!(at_line_5.kind, SymbolKind::Impl);
}

#[tokio::test]
async fn non_rust_files_produce_no_symbols() {
    let (_td, idx) = seeded().await;
    // README.md contains the literal text "Foo" and "State" but those
    // must not spawn symbols — only the rust extractor runs.
    let readme_hits: Vec<_> = idx
        .by_name("Foo", 10)
        .await
        .unwrap()
        .into_iter()
        .filter(|s| s.path.ends_with("README.md"))
        .collect();
    assert!(
        readme_hits.is_empty(),
        "non-rust files must not emit symbols"
    );
}
