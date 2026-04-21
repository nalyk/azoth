//! Symbol retrieval — Sprint 2 (tree-sitter, Rust-only for v2).
//!
//! Lives in `azoth-core` as a trait-only surface so downstream embedders
//! can depend on the interface without pulling tree-sitter. The concrete
//! `SqliteSymbolIndex` impl lives in `azoth-repo`.
//!
//! ## Design decisions (resolved against the v2 plan's §Sprint 2 ambiguities)
//!
//! - **SymbolKind** — plan text had dup tokens (`Module`+`Mod`, `Function`+`Fn`).
//!   The kept set is exactly: `Function | Struct | Enum | EnumVariant |
//!   Trait | Impl | Module | Const`. `EnumVariant` is a late addition so
//!   `by_name("Ready")` lands on the variant, not just the enum.
//! - **Impl name** — tree-sitter's `impl_item` has no `name` field; it has
//!   `type` (what is being implemented on) and optional `trait`. We store
//!   `name = <type text>` so `by_name("MyStruct")` naturally hits both
//!   the `struct_item` and its `impl_item`s. Future work can add a
//!   `qualifier` column for trait-impl disambiguation.
//! - **SymbolId** — `INTEGER PRIMARY KEY AUTOINCREMENT` on the SQLite side;
//!   ephemeral across sessions. IDs never get baked into JSONL events as
//!   durable keys — that would violate invariant #1 (transcript is not
//!   memory; state rebuilds from durable content). Events carry the
//!   *query-time* ID list only.
//! - **digest** — stored at write time for debugging "did the content
//!   actually change"; mtime-gating (Sprint 1's 4-phase pipeline) is the
//!   primary invalidation primary. See the SqliteSymbolIndex docs.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::RetrievalError;

/// Stable, session-ephemeral handle to a symbol row. Backed by SQLite's
/// `INTEGER PRIMARY KEY AUTOINCREMENT`; never persisted to JSONL as a
/// durable reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SymbolId(pub i64);

impl SymbolId {
    pub fn get(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for SymbolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sym_{}", self.0)
    }
}

/// What kind of symbol we extracted. Kept small & language-agnostic so
/// non-Rust grammars (v2.1) can map onto the same enum without churn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Struct,
    Enum,
    EnumVariant,
    Trait,
    Impl,
    Module,
    Const,
    // v2.1 additions for Py/TS/Go grammars. Serde `rename_all =
    // "snake_case"` keeps pre-2.1 JSONL deserialising — older sessions
    // carry only the original eight variants. `type_alias` is the
    // two-word wire tag.
    Class,
    Method,
    Interface,
    TypeAlias,
    Decorator,
    Package,
}

impl SymbolKind {
    /// Stable wire tag for SQLite storage. Persisted — do NOT change the
    /// strings without a schema migration.
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::EnumVariant => "enum_variant",
            SymbolKind::Trait => "trait",
            SymbolKind::Impl => "impl",
            SymbolKind::Module => "module",
            SymbolKind::Const => "const",
            SymbolKind::Class => "class",
            SymbolKind::Method => "method",
            SymbolKind::Interface => "interface",
            SymbolKind::TypeAlias => "type_alias",
            SymbolKind::Decorator => "decorator",
            SymbolKind::Package => "package",
        }
    }

    /// Inverse of `as_str`. Named `from_wire` to avoid colliding with
    /// the `std::str::FromStr` trait method (clippy::should_implement_trait).
    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "function" => SymbolKind::Function,
            "struct" => SymbolKind::Struct,
            "enum" => SymbolKind::Enum,
            "enum_variant" => SymbolKind::EnumVariant,
            "trait" => SymbolKind::Trait,
            "impl" => SymbolKind::Impl,
            "module" => SymbolKind::Module,
            "const" => SymbolKind::Const,
            "class" => SymbolKind::Class,
            "method" => SymbolKind::Method,
            "interface" => SymbolKind::Interface,
            "type_alias" => SymbolKind::TypeAlias,
            "decorator" => SymbolKind::Decorator,
            "package" => SymbolKind::Package,
            _ => return None,
        })
    }
}

/// A single extracted symbol. `id` is the SQLite rowid — stable within one
/// index session, not a durable JSONL key. `parent_id` encodes the
/// enclosing symbol (method → impl, variant → enum) in the same table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub id: SymbolId,
    pub name: String,
    pub kind: SymbolKind,
    /// Relative path, UTF-8. Matches `documents.path`.
    pub path: String,
    /// 1-based line numbers for parity with tools::repo_read / ripgrep.
    pub start_line: u32,
    pub end_line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<SymbolId>,
    /// Stable language tag mirroring `documents.language` (`rust`, etc.).
    pub language: String,
    /// Chronon CP-3: Unix epoch seconds — source mtime at index
    /// time (read from `documents.mtime_nanos / 1e9`). `None` on
    /// pre-CP-3 sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_mtime: Option<u64>,
}

/// Retrieval surface. Two affordances cover what Sprint 4's composite
/// collector needs:
/// - `by_name` — name-driven lookup, used when the driver asks the kernel
///   for symbols matching a user-specified identifier.
/// - `enclosing` — point-query: which symbol contains `path:line`? Used
///   to resolve a span hit back to its enclosing function / impl.
#[async_trait]
pub trait SymbolRetrieval: Send + Sync {
    async fn by_name(&self, name: &str, limit: usize) -> Result<Vec<Symbol>, RetrievalError>;

    async fn enclosing(&self, path: &str, line: u32) -> Result<Option<Symbol>, RetrievalError>;
}

/// v2 Sprint 2 default. Returns nothing. The real impl
/// (`azoth_repo::code_graph::SqliteSymbolIndex`) is opt-in.
pub struct NullSymbolRetrieval;

#[async_trait]
impl SymbolRetrieval for NullSymbolRetrieval {
    async fn by_name(&self, _name: &str, _limit: usize) -> Result<Vec<Symbol>, RetrievalError> {
        Ok(Vec::new())
    }

    async fn enclosing(&self, _path: &str, _line: u32) -> Result<Option<Symbol>, RetrievalError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_kind_wire_round_trips() {
        for k in [
            SymbolKind::Function,
            SymbolKind::Struct,
            SymbolKind::Enum,
            SymbolKind::EnumVariant,
            SymbolKind::Trait,
            SymbolKind::Impl,
            SymbolKind::Module,
            SymbolKind::Const,
            // v2.1
            SymbolKind::Class,
            SymbolKind::Method,
            SymbolKind::Interface,
            SymbolKind::TypeAlias,
            SymbolKind::Decorator,
            SymbolKind::Package,
        ] {
            let s = k.as_str();
            assert_eq!(SymbolKind::from_wire(s), Some(k), "tag {s} must round-trip");
        }
        assert_eq!(SymbolKind::from_wire("not_a_kind"), None);
    }

    /// Pre-2.1 sessions must still deserialise unchanged — asserts the
    /// serde `rename_all = "snake_case"` contract is stable for every
    /// original variant. Complements `symbol_kind_wire_round_trips`
    /// by exercising the `serde_json` surface directly.
    #[test]
    fn pre_2_1_serde_tags_deserialize() {
        for (tag, want) in [
            ("\"function\"", SymbolKind::Function),
            ("\"struct\"", SymbolKind::Struct),
            ("\"enum\"", SymbolKind::Enum),
            ("\"enum_variant\"", SymbolKind::EnumVariant),
            ("\"trait\"", SymbolKind::Trait),
            ("\"impl\"", SymbolKind::Impl),
            ("\"module\"", SymbolKind::Module),
            ("\"const\"", SymbolKind::Const),
        ] {
            let got: SymbolKind = serde_json::from_str(tag).expect("tag must deserialise");
            assert_eq!(got, want, "tag {tag}");
        }
    }

    /// v2.1 variants round-trip via serde with the documented tags.
    #[test]
    fn v2_1_serde_tags_round_trip() {
        for (tag, want) in [
            ("\"class\"", SymbolKind::Class),
            ("\"method\"", SymbolKind::Method),
            ("\"interface\"", SymbolKind::Interface),
            ("\"type_alias\"", SymbolKind::TypeAlias),
            ("\"decorator\"", SymbolKind::Decorator),
            ("\"package\"", SymbolKind::Package),
        ] {
            let got: SymbolKind = serde_json::from_str(tag).expect("tag must deserialise");
            assert_eq!(got, want);
            let re = serde_json::to_string(&got).unwrap();
            assert_eq!(re, tag);
        }
    }

    #[tokio::test]
    async fn null_retrieval_returns_empty() {
        let n = NullSymbolRetrieval;
        assert!(n.by_name("anything", 10).await.unwrap().is_empty());
        assert!(n.enclosing("src/x.rs", 1).await.unwrap().is_none());
    }

    #[test]
    fn symbol_id_display() {
        assert_eq!(SymbolId(42).to_string(), "sym_42");
    }
}
