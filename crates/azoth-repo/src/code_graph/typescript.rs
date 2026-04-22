//! tree-sitter-typescript 0.21 symbol extractor.
//!
//! Two grammar flavours ship in the `tree-sitter-typescript` crate:
//! `language_typescript()` handles `.ts` / `.d.ts`; `language_tsx()`
//! handles `.tsx` (TypeScript + JSX). The extractor is flavour-blind —
//! the dispatcher picks the right parser at construction time via
//! `code_graph::parser_for` (driven by `parser_key` from the file path)
//! and the walker works on whichever tree it's handed.
//!
//! Walker shape mirrors [`super::python`] and [`super::rust`]: iterative
//! pre-order traversal on an explicit stack, classify each node, push
//! an [`ExtractedSymbol`] on match, thread `parent_idx` so
//! `class → method` linkage lands in a single pass. Iterative for the
//! same stack-safety reason spelled out in the Python walker's
//! docstring — a 1 MiB `.ts` file of `(` characters encodes ~1M nodes,
//! well past the 8 MiB Linux default stack at ~256 B/frame.
//!
//! ## Node types emitted (v2.1-C scope)
//!
//! | Node                                 | Classification |
//! |--------------------------------------|----------------|
//! | `function_declaration`               | `Function`     |
//! | `generator_function_declaration`     | `Function`     |
//! | `class_declaration`                  | `Class`        |
//! | `abstract_class_declaration`         | `Class`        |
//! | `method_definition`                  | `Method`       |
//! | `abstract_method_signature`          | `Method`       |
//! | `interface_declaration`              | `Interface`    |
//! | `type_alias_declaration`             | `TypeAlias`    |
//! | `enum_declaration`                   | `Enum`         |
//!
//! `export_statement` and `ambient_declaration` wrappers are not
//! classified themselves — the walker descends through them into the
//! wrapped declaration, so `export function foo() {}` still yields a
//! `Function` symbol named `foo`.
//!
//! ## Out-of-scope / caveats
//!
//! - **JavaScript** (`.js`/`.jsx`): explicitly excluded from v2.1
//!   detection (see `code_graph::detect_language`). Not our grammar.
//! - **`namespace` / `module` declarations**: not emitted as symbols.
//!   TypeScript's namespace system is largely a type-level artifact,
//!   and inner members (functions, classes) surface on their own via
//!   normal descent. A future PR could add a `Namespace` variant if
//!   retrieval coverage demands it.
//! - **Arrow functions assigned to `const`**: not emitted. The grammar
//!   models `const f = (x) => x` as a `lexical_declaration` containing
//!   an `arrow_function`, and naming requires walking through the
//!   variable declarator. Skipped in 2.1-C to keep the node-shape set
//!   tight; can be added without schema churn.
//! - **Decorators**: TypeScript decorators (`@foo`) are not emitted.
//!   Python emits them because the runtime-meaning maps cleanly to a
//!   Decorator symbol; TypeScript decorators are more varied in
//!   usage (class vs method vs accessor) and a future PR can wire them
//!   distinctly rather than squeezing them into the Python semantics.

use super::common::{line_range, name_via_field, short_digest};
use super::ExtractedSymbol;
use azoth_core::retrieval::SymbolKind;
use tree_sitter::{Node, Parser, Tree};

/// Build a parser pre-configured for the `.ts`/`.d.ts` grammar. The
/// caller owns the instance and reuses it across every `.ts` file in a
/// reindex pass. See `rust.rs` module docs for the reuse rationale.
pub fn typescript_parser_ts() -> Result<Parser, super::ExtractError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_typescript::language_typescript())
        .map_err(|_| super::ExtractError::Language)?;
    Ok(parser)
}

/// Build a parser pre-configured for the `.tsx` grammar (TypeScript +
/// JSX). Kept distinct from the `.ts` parser because the grammars are
/// genuinely different internally — the `.ts` grammar treats `<T>(x)`
/// as a type assertion and can't parse JSX elements without erroring.
pub fn typescript_parser_tsx() -> Result<Parser, super::ExtractError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_typescript::language_tsx())
        .map_err(|_| super::ExtractError::Language)?;
    Ok(parser)
}

/// Parse `src` with the caller-supplied TypeScript parser and extract
/// every symbol the grammar recognises. Works for both `.ts` and
/// `.tsx` trees — the node-kind set the classifier handles is shared
/// across both flavours.
pub fn extract_typescript(
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
/// `method_definition` nested inside a `function_declaration` still
/// classifies as `Method`. TypeScript grammar distinguishes methods by
/// node kind (`method_definition` / `abstract_method_signature`) rather
/// than by context, so the flag is mostly redundant for this walker —
/// but it's threaded the same way as in Python/Rust for parity with
/// the shared walker shape and to keep the door open for future
/// contextual classification (e.g. arrow functions bound as class
/// fields, which the grammar models as public_field_definition).
///
/// # Why iterative (parity with python.rs/rust.rs)
///
/// PR #20 round 5 converted both Python and Rust walkers to iterative
/// after codex P2 flagged the recursive shape as stack-overflow-able
/// under adversarial input. TypeScript inherits the same attack surface
/// — a 1 MiB `.ts` file full of `(` encodes ~1M nodes needing ~256 MB
/// stack at ~256 B/frame, 32× over the default 8 MB Linux thread stack
/// and 128× over the 2 MB test-thread default. The iterative walker
/// stays O(1) in stack depth; pre-order is preserved by pushing
/// children in reverse so pop order matches recursive descent.
fn walk(root: Node<'_>, bytes: &[u8], out: &mut Vec<ExtractedSymbol>) {
    let mut stack: Vec<(Node<'_>, Option<usize>, bool)> = vec![(root, None, false)];
    // Single reused TreeCursor across every node. Gemini round-7 MED
    // on PR #20 `cf4c6ac`: `node.walk()` per iteration allocates a
    // fresh TSTreeCursor C struct; `cursor.reset(node)` avoids the
    // per-node allocation without changing complexity. Re-initialised
    // at the head of every children-push block, so no stale state
    // from the prior node's sibling walk leaks forward.
    let mut cursor = root.walk();
    while let Some((node, parent_idx, enclosing_container_is_class)) = stack.pop() {
        let me = classify(node, bytes);

        let (next_parent, next_container_is_class) = if let Some((name, kind)) = me {
            let (start_line, end_line) = line_range(&node);
            out.push(ExtractedSymbol {
                name,
                kind,
                start_line,
                end_line,
                parent_idx,
                digest: short_digest(&node, bytes),
            });
            let idx = out.len() - 1;
            let child_container = match kind {
                SymbolKind::Class | SymbolKind::Interface => true,
                // Function/method bodies open a non-class scope —
                // nested functions are closures, not methods of an
                // outer class. Holds for both `Function` and `Method`.
                SymbolKind::Function | SymbolKind::Method => false,
                _ => enclosing_container_is_class,
            };
            (Some(idx), child_container)
        } else {
            (parent_idx, enclosing_container_is_class)
        };

        // Push children in REVERSE so pop order preserves pre-order.
        // TreeCursor forward walk + in-place reverse of the newly-
        // added tail. O(N) per parent, zero heap allocations. See
        // `python.rs::walk` for the full (a)/(b)/(c) tradeoff
        // analysis behind this exact shape.
        let stack_tail_start = stack.len();
        cursor.reset(node);
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

fn classify(node: Node<'_>, bytes: &[u8]) -> Option<(String, SymbolKind)> {
    match node.kind() {
        "function_declaration" | "generator_function_declaration" => {
            name_via_field(&node, "name", bytes).map(|n| (n, SymbolKind::Function))
        }
        "class_declaration" | "abstract_class_declaration" => {
            name_via_field(&node, "name", bytes).map(|n| (n, SymbolKind::Class))
        }
        "method_definition" | "abstract_method_signature" => {
            name_via_field(&node, "name", bytes).map(|n| (n, SymbolKind::Method))
        }
        "interface_declaration" => {
            name_via_field(&node, "name", bytes).map(|n| (n, SymbolKind::Interface))
        }
        "type_alias_declaration" => {
            name_via_field(&node, "name", bytes).map(|n| (n, SymbolKind::TypeAlias))
        }
        "enum_declaration" => name_via_field(&node, "name", bytes).map(|n| (n, SymbolKind::Enum)),
        _ => None,
    }
}
