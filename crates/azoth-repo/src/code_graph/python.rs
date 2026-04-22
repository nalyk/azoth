//! tree-sitter-python 0.21 symbol extractor.
//!
//! Walker shape mirrors `rust.rs` for consistency: iterative
//! pre-order traversal over an explicit stack, classify each node,
//! push an [`ExtractedSymbol`] when recognised, threading
//! `parent_idx` so method → class linkage lands in one pass. Both
//! walkers converted to iterative in PR #20 round 5 (see [`walk`]
//! docstring for the codex P2 stack-overflow analysis that motivated
//! the change).
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
    walk(tree.root_node(), bytes, &mut out);
    Ok(out)
}

/// Iterative pre-order traversal of the parse tree.
///
/// `enclosing_container_is_class` determines whether a
/// `function_definition` classifies as `Method` or `Function`; it flips
/// to `true` on `class_definition` entry and resets to `false` on any
/// `function_definition` entry (nested defs are closures, not methods
/// of the outer class).
///
/// # Why iterative (PR #20 round 5)
///
/// Round-4 of the review cycle shipped with a recursive walker and a
/// long docstring arguing recursion was safe because CPython caps its
/// own recursion at 1000 frames. Codex (P2 on `482851e`) correctly
/// pointed out that azoth indexes **raw repo text, not
/// CPython-compilable Python**: a 1 MiB `.py` file filled with `(`
/// characters encodes ~1M `parenthesized_expression` nodes, and
/// `DEFAULT_MAX_FILE_BYTES = 1_048_576` is the actual attack budget.
/// At ≈ 256 B/frame, 1M frames need ≈ 256 MB of stack — 32× over the
/// 8 MB Linux default, 128× over the 2 MB test-thread default. I was
/// wrong in round 4; the walker stays O(1) in stack depth from here
/// on. Pre-order traversal is preserved by pushing children in
/// reverse (pop order then matches recursive order).
///
/// Sibling: [`crate::code_graph::rust::walk`] converted to iterative
/// in the same round for identical reasoning (malicious `.rs` file of
/// `{` or `(` would stack-overflow the old recursive walker).
fn walk(root: Node<'_>, bytes: &[u8], out: &mut Vec<ExtractedSymbol>) {
    let mut stack: Vec<(Node<'_>, Option<usize>, bool)> = vec![(root, None, false)];
    while let Some((node, parent_idx, enclosing_container_is_class)) = stack.pop() {
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
                // Every function body resets the flag — nested
                // `def`s are closures, not methods. Holds for both
                // `Function` and `Method` classifications.
                SymbolKind::Function | SymbolKind::Method => false,
                // Decorators don't open a new scope; carry the
                // parent's container flag forward.
                _ => enclosing_container_is_class,
            };
            (Some(idx), child_container)
        } else {
            (parent_idx, enclosing_container_is_class)
        };

        // Push children in REVERSE order so pop order preserves
        // pre-order traversal. Two allocation-free ways this COULD
        // be written and one allocation-heavy way to avoid:
        //
        //   (a) Collect via `Vec<Node>` + `.rev()` — 1 heap alloc per
        //       parent (old round-5 shape; gemini MED on `ce414de`
        //       correctly flagged this overhead on large files).
        //   (b) `for i in (0..node.child_count()).rev() { node.child(i) }`
        //       — gemini's suggestion. Looks zero-alloc, but
        //       `ts_node_child(n, i)` in tree-sitter 0.22.6 is O(i)
        //       internally (linear sibling walk via
        //       `ts_node_iterate_children`). Collecting N children
        //       this way is O(N²) per parent, a regression on files
        //       with many siblings under one node.
        //   (c) TreeCursor forward walk + in-place reverse of the
        //       stack tail. O(N) per parent via cursor, zero heap
        //       allocations, amortized O(1) Vec growth on the
        //       shared stack.
        //
        // Going with (c). Extends the shared stack directly, then
        // reverses the newly-added tail so subsequent pops match
        // pre-order. Same emission sequence as the collect+rev()
        // variant — `parent_idx` values and digest ordering remain
        // byte-identical to round 5.
        let stack_tail_start = stack.len();
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                stack.push((cursor.node(), next_parent, next_container_is_class));
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        stack[stack_tail_start..].reverse();
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
/// Iterative for the same stack-safety reason as [`walk`]: a
/// `.py` file with a decorator like `@(((((...wrap...)))))` at
/// 10 000 nesting levels would otherwise overflow the thread stack.
/// Pre-order is preserved by pushing children in reverse (pop order
/// matches the recursive descent that used to live here).
fn first_leaf_identifier(root: Node<'_>, bytes: &[u8]) -> Option<String> {
    let mut stack: Vec<Node<'_>> = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "identifier" {
            return node.utf8_text(bytes).ok().map(str::to_owned);
        }
        // Same TreeCursor-forward + in-place-reverse pattern as
        // [`walk`] — see that function's children-push rationale.
        let stack_tail_start = stack.len();
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                stack.push(cursor.node());
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        stack[stack_tail_start..].reverse();
    }
    None
}
