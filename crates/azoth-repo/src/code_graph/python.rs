//! tree-sitter-python 0.21 symbol extractor.
//!
//! Walker shape mirrors `rust.rs` for consistency: recurse once,
//! classify each node, push an [`ExtractedSymbol`] when recognised,
//! threading `parent_idx` so method → class linkage lands in one pass.
//!
//! Node types emitted (v2.1 scope):
//!
//! | Node                    | Classification                        |
//! |-------------------------|---------------------------------------|
//! | `function_definition`   | `Method` if enclosing container is a  |
//! |                         | class, else `Function`                |
//! | `class_definition`      | `Class`                               |
//! | `decorator`             | `Decorator` (first leaf identifier)   |
//!
//! ## Container-scope propagation
//!
//! The walker carries `enclosing_container_is_class: bool`. A
//! `class_definition` flips it to `true` for its subtree; every
//! `function_definition` flips it back to `false` for its body, so a
//! nested `def` inside a method classifies as `Function` (closure),
//! not `Method`. This matches Python semantics — a `self`-taking
//! method and a free closure are different kinds of callable.
//!
//! ## Decorator name extraction
//!
//! Python decorators can be `@foo`, `@foo.bar`, `@foo()`,
//! `@foo.bar.baz()`, or even full call-expressions. Rather than
//! hand-rolling an expression matcher, the extractor descends into the
//! decorator subtree until it hits the first `identifier` leaf and
//! uses that as the symbol name. That yields:
//!
//! | Source                  | Emitted name |
//! |-------------------------|--------------|
//! | `@wrap`                 | `wrap`       |
//! | `@pkg.inner.outer`      | `pkg`        |
//! | `@cache(maxsize=1)`     | `cache`      |
//! | `@pkg.decorator(x)`     | `pkg`        |
//!
//! First-leaf semantics (outermost qualifier) is stable across grammar
//! versions; inner names (`outer`, `baz`) would require walking the
//! rightmost subtree on every grammar update. Consumers that need the
//! innermost name can follow the attribute chain themselves.
//!
//! ## Macros caveat (parity with rust.rs)
//!
//! Runtime metaprogramming (`type(...)`, `exec(...)`, monkey-patching,
//! `__init_subclass__` tricks) is invisible to tree-sitter. Documented
//! rather than papered over.

use super::rust::ExtractedSymbol;
use azoth_core::retrieval::SymbolKind;
use sha2::{Digest, Sha256};
use tree_sitter::{Node, Parser, Tree};

/// Build a tree-sitter [`Parser`] pre-configured for Python 0.21. The
/// caller owns the instance and reuses it across every file in a
/// reindex pass (see `rust.rs` module docs for rationale).
pub fn python_parser() -> Result<Parser, super::ExtractError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::language())
        .map_err(|_| super::ExtractError::Language)?;
    Ok(parser)
}

/// Parse `src` with the caller-supplied Python parser and extract
/// every symbol the grammar recognises. The parser is expected to have
/// Python set as its language already (see [`python_parser`]).
pub fn extract_python(
    parser: &mut Parser,
    src: &str,
) -> Result<Vec<ExtractedSymbol>, super::ExtractError> {
    let tree: Tree = parser.parse(src, None).ok_or(super::ExtractError::Parse)?;
    let bytes = src.as_bytes();
    let mut out: Vec<ExtractedSymbol> = Vec::new();
    walk(tree.root_node(), bytes, None, false, &mut out);
    Ok(out)
}

/// Recursive descent. `enclosing_container_is_class` determines whether
/// a `function_definition` classifies as `Method` or `Function`; it is
/// flipped to `true` on `class_definition` entry and reset to `false`
/// on `function_definition` entry (nested defs are closures, not
/// methods of the outer class).
fn walk(
    node: Node<'_>,
    bytes: &[u8],
    parent_idx: Option<usize>,
    enclosing_container_is_class: bool,
    out: &mut Vec<ExtractedSymbol>,
) {
    let me = classify(node, bytes, enclosing_container_is_class);

    let (next_parent, next_container_is_class) = if let Some((name, kind)) = me {
        let (s, e) = line_range(&node);
        out.push(ExtractedSymbol {
            name,
            kind,
            start_line: s,
            end_line: e,
            parent_idx,
            digest: short_digest(&node, bytes),
        });
        let idx = out.len() - 1;
        let child_container = match kind {
            SymbolKind::Class => true,
            // Every function body resets the flag — nested `def`s are
            // closures, not methods. Holds for both `Function` and
            // `Method` classifications.
            SymbolKind::Function | SymbolKind::Method => false,
            // Decorators don't open a new scope; carry the parent's
            // container flag forward.
            _ => enclosing_container_is_class,
        };
        (Some(idx), child_container)
    } else {
        (parent_idx, enclosing_container_is_class)
    };

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, bytes, next_parent, next_container_is_class, out);
    }
}

fn classify(
    node: Node<'_>,
    bytes: &[u8],
    enclosing_container_is_class: bool,
) -> Option<(String, SymbolKind)> {
    match node.kind() {
        "function_definition" => {
            let name = name_via_field(&node, "name", bytes)?;
            let kind = if enclosing_container_is_class {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            Some((name, kind))
        }
        "class_definition" => name_via_field(&node, "name", bytes).map(|n| (n, SymbolKind::Class)),
        "decorator" => first_leaf_identifier(node, bytes).map(|n| (n, SymbolKind::Decorator)),
        _ => None,
    }
}

fn name_via_field(node: &Node<'_>, field: &str, bytes: &[u8]) -> Option<String> {
    node.child_by_field_name(field)
        .and_then(|c| c.utf8_text(bytes).ok())
        .map(str::to_owned)
}

/// Descend into a decorator subtree and return the first `identifier`
/// leaf found in pre-order traversal. Handles every decorator shape
/// the Python grammar emits uniformly (bare, attribute, call). See
/// module docs for the naming semantics (outermost qualifier).
fn first_leaf_identifier(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    if node.kind() == "identifier" {
        return node.utf8_text(bytes).ok().map(str::to_owned);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(name) = first_leaf_identifier(child, bytes) {
            return Some(name);
        }
    }
    None
}

fn line_range(node: &Node<'_>) -> (u32, u32) {
    let s = node.start_position().row;
    let e = node.end_position().row;
    // 1-based lines, parity with `rust.rs`.
    ((s as u32).saturating_add(1), (e as u32).saturating_add(1))
}

/// SHA-256 digest of the node's source bytes, truncated to 16 hex
/// chars. Parity with `rust.rs::short_digest` — see that doc for
/// rationale.
fn short_digest(node: &Node<'_>, bytes: &[u8]) -> String {
    let start = node.start_byte();
    let end = node.end_byte().min(bytes.len());
    let slice = &bytes[start..end];
    let mut h = Sha256::new();
    h.update(slice);
    let digest = h.finalize();
    hex::encode(&digest[..8])
}
