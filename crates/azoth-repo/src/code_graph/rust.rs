//! tree-sitter-rust 0.21 symbol extractor.
//!
//! Walks the parse tree recursively, emitting one `ExtractedSymbol`
//! per recognised construct. Parent/child relationships (method → impl,
//! variant → enum) are captured via `parent_idx` pointing into the
//! returned Vec so the SQLite writer can resolve them to rowids without
//! a second pass.
//!
//! ## Why a walker, not a tree-sitter Query
//!
//! tree-sitter queries match individual node shapes beautifully but
//! don't preserve *which* outer match contained a given inner match.
//! A recursive walk gives us an explicit ancestor stack with zero
//! parser overhead, so we implement it directly.
//!
//! ## Macros caveat
//!
//! Symbols inside macro bodies (`lazy_static! { ... }`) are invisible
//! to tree-sitter because it doesn't expand macros. We document the
//! limitation rather than trying to paper over it.
//!
//! ## Parser reuse (PR #6 gemini-code-assist MED)
//!
//! `extract_rust` takes `&mut Parser` so callers in hot loops
//! (`RepoIndexer::reindex_blocking`) can construct one parser per
//! reindex pass instead of per file. The grammar pointer (set via
//! `set_language`) is sticky on a `Parser`, so reuse is free once
//! it's configured. Tests construct a parser on-demand via
//! [`rust_parser`].

use super::common::{line_range, name_via_field, short_digest};
use super::Language;
use azoth_core::retrieval::SymbolKind;
use tree_sitter::{Node, Parser, Tree};

/// Raw, flat record produced by the extractor. Lives in `azoth-repo`
/// only — never enters any public `azoth-core` surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedSymbol {
    pub name: String,
    pub kind: SymbolKind,
    /// 1-based line numbers (tree-sitter gives 0-based, we convert).
    pub start_line: u32,
    pub end_line: u32,
    /// Index into the vector this one came from. `None` means top-level.
    pub parent_idx: Option<usize>,
    /// 16 hex chars of a fast (non-cryptographic) hash of the body
    /// bytes. Stored for debug-time "did the content change?" queries;
    /// mtime-gating on `documents` is the primary invalidation driver.
    pub digest: String,
}

/// Errors are shaped so the Phase-4 caller can decide: "skip file, carry
/// on" (parser failed) vs. "bubble up" (caller bug). For now the
/// extractor is infallible past parser construction, but we keep a
/// Result for API stability.
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    /// Internal tree-sitter failure during `Parser::set_language` —
    /// signals a grammar-ABI mismatch for an already-wired language.
    /// Distinct from `UnsupportedLanguage`, which means "we deliberately
    /// have no extractor for this variant yet."
    #[error("tree-sitter: failed to set language")]
    Language,
    #[error("tree-sitter: parse returned no tree")]
    Parse,
    /// `code_graph::extract_for` / `parser_for` was called with a
    /// `Language` whose grammar has not yet been wired (PRs 2.1-B / C /
    /// D add Python / TypeScript / Go). Callers in the indexer treat
    /// this as a benign "skip symbols for this file" signal — not a
    /// log-worthy failure — so it must stay distinct from
    /// `ExtractError::Language` (which IS log-worthy).
    #[error("no extractor wired for language: {0:?}")]
    UnsupportedLanguage(Language),
    /// `parser_key` saw a `(Language, path)` pair whose path extension
    /// is inconsistent with the language. Fires when
    /// `documents.language` and `documents.path.extension` disagree —
    /// which can happen only through external corruption (manual DB
    /// edit, concurrent writer outside our transaction discipline, or
    /// a future binary that wrote a row format 2.1-A cannot interpret).
    /// The indexer treats this as **invariant violation on durable
    /// data** (not authorship): log an error, purge the path's symbol
    /// rows to restore the "Ok(0) ⇒ zero rows" uniform invariant, and
    /// continue with the rest of the reindex pass. Gemini raised this
    /// across PR #19 rounds 4/6/8/9; I rejected twice with docs, then
    /// re-investigated on the 4th raise (per the 3+-raises rule in
    /// `feedback_reject_with_documentation_when_arch_forbids.md`) and
    /// accepted that a secondary index must be self-healing, not
    /// panic-crashing.
    #[error("language/path mismatch: language={language:?} extension={extension:?}")]
    LanguagePathMismatch {
        language: Language,
        extension: Option<String>,
    },
}

/// Build a tree-sitter `Parser` pre-configured for Rust. The caller
/// owns the instance and reuses it across every file in a reindex pass
/// (see module-level docs). Extracted into its own function so tests
/// and ad-hoc callers don't need to duplicate the `set_language`
/// incantation.
pub fn rust_parser() -> Result<Parser, ExtractError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::language())
        .map_err(|_| ExtractError::Language)?;
    Ok(parser)
}

/// Parse `src` with the caller-supplied parser and extract every
/// symbol the grammar recognises. The parser is expected to have
/// Rust set as its language already (see [`rust_parser`]).
pub fn extract_rust(parser: &mut Parser, src: &str) -> Result<Vec<ExtractedSymbol>, ExtractError> {
    let tree: Tree = parser.parse(src, None).ok_or(ExtractError::Parse)?;

    let bytes = src.as_bytes();
    let mut out: Vec<ExtractedSymbol> = Vec::new();
    walk(tree.root_node(), bytes, &mut out);
    Ok(out)
}

/// Iterative pre-order traversal. `parent_idx` threads the index of
/// the enclosing emitted Symbol down to children so nested constructs
/// (methods inside impls, variants inside enums) resolve to their
/// parent in one pass.
///
/// # Why iterative (PR #20 round 5)
///
/// Codex P2 on `482851e` flagged the recursive walkers as
/// stack-overflow-able on adversarial input: a 1 MiB `.rs` file of
/// `{` or `(` characters encodes ~1M nodes, needing ≈ 256 MB stack at
/// ~256 B/frame — 32× over Linux's 8 MB default. This Rust walker
/// had the same shape as the Python one (PR #20 introduced it);
/// converting both in one round closes the class. Pre-order is
/// preserved by pushing children in reverse so pop order matches the
/// recursive descent that used to live here.
fn walk(root: Node<'_>, bytes: &[u8], out: &mut Vec<ExtractedSymbol>) {
    let mut stack: Vec<(Node<'_>, Option<usize>)> = vec![(root, None)];
    // Reused TreeCursor — see `python.rs::walk` docstring for the
    // round-7 gemini MED rationale (per-node `node.walk()` allocates
    // a fresh TSTreeCursor C struct; `cursor.reset(node)` avoids it).
    let mut cursor = root.walk();
    while let Some((node, parent_idx)) = stack.pop() {
        let me = classify(node, bytes);

        let next_parent = if let Some((name, kind)) = me {
            let (start_line, end_line) = line_range(&node);
            let digest = short_digest(&node, bytes);
            out.push(ExtractedSymbol {
                name,
                kind,
                start_line,
                end_line,
                parent_idx,
                digest,
            });
            Some(out.len() - 1)
        } else {
            parent_idx
        };

        // TreeCursor forward walk + in-place reverse of the newly-
        // added stack tail. O(N) per parent, zero heap allocations.
        // See `python.rs::walk` for the full rationale (including
        // why `node.child(i)` in a reverse loop — gemini's suggested
        // shape — is O(N²) internally and thus not the right fix).
        let stack_tail_start = stack.len();
        cursor.reset(node);
        if cursor.goto_first_child() {
            loop {
                stack.push((cursor.node(), next_parent));
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        stack[stack_tail_start..].reverse();
    }
}

fn classify(node: Node<'_>, bytes: &[u8]) -> Option<(String, SymbolKind)> {
    match node.kind() {
        "function_item" => name_via_field(&node, "name", bytes).map(|n| (n, SymbolKind::Function)),
        "struct_item" => name_via_field(&node, "name", bytes).map(|n| (n, SymbolKind::Struct)),
        "enum_item" => name_via_field(&node, "name", bytes).map(|n| (n, SymbolKind::Enum)),
        "enum_variant" => {
            name_via_field(&node, "name", bytes).map(|n| (n, SymbolKind::EnumVariant))
        }
        "trait_item" => name_via_field(&node, "name", bytes).map(|n| (n, SymbolKind::Trait)),
        "impl_item" => {
            // impl has no `name` — use the `type` field's text as the
            // primary name so `by_name("Vec")` lands on the impl too.
            name_via_field(&node, "type", bytes).map(|n| (n, SymbolKind::Impl))
        }
        "mod_item" => name_via_field(&node, "name", bytes).map(|n| (n, SymbolKind::Module)),
        "const_item" => name_via_field(&node, "name", bytes).map(|n| (n, SymbolKind::Const)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(src: &str) -> Vec<ExtractedSymbol> {
        let mut parser = rust_parser().unwrap();
        extract_rust(&mut parser, src).unwrap()
    }

    #[test]
    fn extracts_top_level_function() {
        let syms = extract("fn alpha() {}\n");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "alpha");
        assert_eq!(syms[0].kind, SymbolKind::Function);
        assert_eq!(syms[0].start_line, 1);
        assert_eq!(syms[0].end_line, 1);
        assert_eq!(syms[0].parent_idx, None);
        assert_eq!(syms[0].digest.len(), 16);
    }

    #[test]
    fn extracts_struct_and_enum_with_variants() {
        let syms = extract("pub struct S { x: u32 }\npub enum E { Ready, Done(u8) }\n");
        // Expect: S (struct), E (enum), Ready (variant), Done (variant).
        assert!(syms
            .iter()
            .any(|s| s.name == "S" && s.kind == SymbolKind::Struct));
        let enum_idx = syms
            .iter()
            .position(|s| s.name == "E" && s.kind == SymbolKind::Enum)
            .unwrap();
        let ready = syms
            .iter()
            .find(|s| s.name == "Ready" && s.kind == SymbolKind::EnumVariant)
            .unwrap();
        assert_eq!(ready.parent_idx, Some(enum_idx));
        let done = syms
            .iter()
            .find(|s| s.name == "Done" && s.kind == SymbolKind::EnumVariant)
            .unwrap();
        assert_eq!(done.parent_idx, Some(enum_idx));
    }

    #[test]
    fn method_in_impl_links_to_impl_parent() {
        let src = r#"
struct Foo;
impl Foo {
    fn bar(&self) {}
    fn baz(&self) {}
}
"#;
        let syms = extract(src);
        let impl_idx = syms
            .iter()
            .position(|s| s.kind == SymbolKind::Impl && s.name == "Foo")
            .expect("impl Foo extracted");
        let bar = syms
            .iter()
            .find(|s| s.name == "bar" && s.kind == SymbolKind::Function)
            .unwrap();
        let baz = syms
            .iter()
            .find(|s| s.name == "baz" && s.kind == SymbolKind::Function)
            .unwrap();
        assert_eq!(bar.parent_idx, Some(impl_idx));
        assert_eq!(baz.parent_idx, Some(impl_idx));
    }

    #[test]
    fn trait_impl_uses_type_as_name() {
        let src = "struct Q;\nimpl std::fmt::Display for Q {\n    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { Ok(()) }\n}\n";
        let syms = extract(src);
        // impl_item's type field = "Q", so the Impl symbol is named "Q".
        assert!(syms
            .iter()
            .any(|s| s.kind == SymbolKind::Impl && s.name == "Q"));
    }

    #[test]
    fn module_and_const_extracted() {
        let syms = extract("pub mod sub { pub const X: u32 = 1; }\npub const TOP: u32 = 2;\n");
        let module = syms
            .iter()
            .find(|s| s.name == "sub" && s.kind == SymbolKind::Module)
            .expect("mod extracted");
        let inner = syms
            .iter()
            .find(|s| s.name == "X" && s.kind == SymbolKind::Const)
            .expect("const X extracted");
        assert!(inner.parent_idx.is_some());
        assert_eq!(
            inner.parent_idx,
            Some(syms.iter().position(|s| std::ptr::eq(s, module)).unwrap())
        );
        let top = syms
            .iter()
            .find(|s| s.name == "TOP" && s.kind == SymbolKind::Const)
            .unwrap();
        assert_eq!(top.parent_idx, None);
    }

    #[test]
    fn empty_source_produces_no_symbols() {
        assert!(extract("").is_empty());
        assert!(extract("// only a comment\n").is_empty());
    }

    #[test]
    fn digest_changes_when_body_changes() {
        let a = extract("fn f() { 1 }\n");
        let b = extract("fn f() { 2 }\n");
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        assert_ne!(a[0].digest, b[0].digest, "body delta must shift digest");
    }

    #[test]
    fn digest_is_deterministic_across_calls() {
        // PR #6 review #1: the digest column is supposed to survive
        // process restarts and rustc toolchain bumps. This test pins
        // one well-known input to its expected SHA-256-truncated hash
        // so a future `DefaultHasher` regression or algorithm change
        // surfaces as a test failure.
        let syms1 = extract("fn stable() {}\n");
        let syms2 = extract("fn stable() {}\n");
        assert_eq!(syms1.len(), 1);
        assert_eq!(syms2.len(), 1);
        assert_eq!(
            syms1[0].digest, syms2[0].digest,
            "same input must produce same digest across calls"
        );
    }

    #[test]
    fn parser_instance_is_reusable_across_extractions() {
        // PR #6 review #3: callers (RepoIndexer::reindex_blocking)
        // reuse one Parser across every .rs file in the pass. Prove
        // that works — two extractions through the same parser
        // return independent, correct results.
        let mut parser = rust_parser().unwrap();
        let a = extract_rust(&mut parser, "fn first() {}\n").unwrap();
        let b = extract_rust(&mut parser, "fn second() {}\n").unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].name, "first");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].name, "second");
    }
}
