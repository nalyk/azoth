//! Symbol graph subsystem — tree-sitter extraction + SQLite storage.
//!
//! Sprint 2 ships the Rust-only extractor. Python / TS / Go grammars
//! are deferred to v2.1 (one-grammar-per-PR). The public API:
//!
//! - [`rust::extract_rust`] — stateless extractor: source text → flat
//!   `ExtractedSymbol` list.
//! - [`index::SqliteSymbolIndex`] — persistent index implementing
//!   `azoth_core::retrieval::SymbolRetrieval`, plus the Phase-4
//!   writer hook `index::replace_symbols_for_path`.

pub mod index;
pub mod rust;

pub use index::{replace_symbols_for_path, SqliteSymbolIndex, SymbolWriter};
pub use rust::{extract_rust, rust_parser, ExtractError, ExtractedSymbol};
