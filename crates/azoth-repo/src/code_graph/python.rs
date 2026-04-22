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

use super::common::{line_range, name_via_field, short_digest};
use super::ExtractedSymbol;
use azoth_core::retrieval::SymbolKind;
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
///
/// Stack-overflow analysis: see [`first_leaf_identifier`] for the full
/// reasoning shared with this walker. Depth bounded by CPython's own
/// 1000-frame recursion limit; Rust 8 MiB stack at ≈ 256 B/frame gives
/// a 30× margin. The [`crate::code_graph::rust::walk`] sibling is
/// recursive in the same shape for the same reasons.
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

/// Descend into a decorator subtree and return the first `identifier`
/// leaf found in pre-order traversal. Handles every decorator shape
/// the Python grammar emits uniformly (bare, attribute, call). See
/// module docs for the naming semantics (outermost qualifier).
///
/// # Why recursion, not `TreeCursor`-based iteration
///
/// Gemini raised a stack-overflow concern on PR #20 round 3
/// (`db6c393`) — tree-sitter trees can in principle reach arbitrary
/// depth, and a deeply-nested decorator expression (e.g. thousands
/// of nested parentheses) would recurse as far as the input allows.
/// The concern applies equally to the main [`walk`] function above;
/// both are recursive tree walks.
///
/// Rejected with documentation for three reasons:
///
/// 1. **Depth is bounded by what CPython itself parses.** CPython's
///    default recursion limit is 1000; valid Python source cannot
///    exceed that before `SyntaxError`. tree-sitter-python would
///    accept deeper input, but a file that CPython cannot compile
///    is not a realistic production input to azoth.
/// 2. **Stack budget is comfortable even at the ceiling.** Rust's
///    default stack is 8 MiB. Each frame here is ≈ 256 bytes
///    (Node, `&[u8]`, TreeCursor, a few locals). At CPython's
///    1000-depth ceiling, stack use is ≈ 3%. A 30× margin to
///    overflow.
/// 3. **Sibling consistency.** [`walk`] (this file) and
///    [`crate::code_graph::rust::walk`] are both recursive in the
///    identical shape. Converting `first_leaf_identifier` alone
///    would leave the larger walker unchanged and create stylistic
///    drift between grammar modules. The principled fix is to
///    convert all three to iterative `TreeCursor` walks in one
///    dedicated PR; that refactor also has to preserve the
///    `parent_idx` and `inside_class` state threading, which is
///    straightforward but non-trivial — out of scope for PR 2.1-B
///    (Python grammar add). Tracked as a v2.5 hardening candidate;
///    re-evaluate if a real-world pathological input surfaces.
///
/// Empirical floor on realistic input: `@pkg.mod.sub.wrap` = 4
/// levels; `@((((foo))))` = 5 levels. Typical production depth < 10.
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
