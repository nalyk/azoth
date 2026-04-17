//! azoth-repo — heavy indexing backends for the repo intelligence moat.
//!
//! v2 Sprint 1 lands:
//! - `indexer::RepoIndexer` — walks the repo via `ignore::WalkBuilder`
//!   (scope-parity with `RipgrepLexicalRetrieval`) and upserts mtime-gated
//!   rows into SQLite FTS5 tables.
//! - `fts::FtsLexicalRetrieval` — drop-in sibling of
//!   `RipgrepLexicalRetrieval`, both impl `azoth_core::retrieval::LexicalRetrieval`.
//!
//! Both share the `.azoth/state.sqlite` file with `SqliteMirror` by opening
//! a separate `rusqlite::Connection`. WAL mode — enabled by the mirror at
//! first open — is persistent on the file, so reads and writes from
//! multiple connections coexist safely. Migrations are idempotent; either
//! side may call `azoth_core::event_store::migrations::run` and the
//! indexer does so defensively on open.
//!
//! Dep-arrow: `azoth-core` stays thin and has zero knowledge of this
//! crate; `azoth (bin)` and future embedders opt in by depending on
//! `azoth-repo` and handing the concrete retrieval impls to the runtime
//! via the trait objects defined in `azoth_core::retrieval`.

pub mod code_graph;
pub mod fts;
pub mod history;
pub mod indexer;

pub use code_graph::{extract_rust, SqliteSymbolIndex};
pub use fts::FtsLexicalRetrieval;
pub use history::{CoEditBuildStats, CoEditError, CoEditGraphRetrieval, PATH_PREFIX};
pub use indexer::{IndexStats, IndexerError, RepoIndexer};
