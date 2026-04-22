//! tree-sitter-go 0.21 symbol extractor.
//!
//! Walker shape mirrors [`super::rust`] / [`super::python`] /
//! [`super::typescript`] for consistency: iterative pre-order traversal
//! on an explicit stack, classify each node, push an
//! [`ExtractedSymbol`] on match, thread `parent_idx` so
//! `interface → method_elem` linkage lands in a single pass. Iterative
//! for the same stack-safety reason spelled out in the Python walker's
//! docstring — a 1 MiB `.go` file full of `(` characters encodes ~1M
//! nodes, well past the 8 MiB Linux default stack at ~256 B/frame.
//!
//! ## Node types emitted (v2.1-D scope)
//!
//! | Grammar node           | Classification                       |
//! |------------------------|--------------------------------------|
//! | `package_clause`       | `Package` (name = `package_identifier` child) |
//! | `function_declaration` | `Function` (name field = `identifier`) |
//! | `method_declaration`   | `Method` (name field = `field_identifier`) |
//! | `method_elem`          | `Method` (interface member)          |
//! | `type_spec`            | `Struct` / `Interface` / `TypeAlias` depending on the `type` field's child kind |
//! | `type_alias`           | `TypeAlias` (Go 1.9+ `type X = Y` syntax) |
//! | `const_spec`           | `Const` × N (one per identifier in the `name` field — `multiple: true` in the grammar, so `const A, B = 1, 2` emits two `Const`s) |
//!
//! ## Two misses the v2.1 plan's D4 draft had
//!
//! Enumerating `node-types.json` before writing the classifier (per
//! memory `pattern_tree_sitter_classifier_enumerate_node_types.md` —
//! PR #22 R1 shipped missing `function_signature`/`method_signature`
//! for exactly this reason) surfaced two grammar realities the plan
//! didn't account for:
//!
//! 1. **`type_alias` is a distinct node kind.** The Go 1.9+ alias
//!    syntax `type X = Y` parses to `type_alias`, NOT `type_spec`.
//!    Classifying only `type_spec` would silently drop every alias
//!    declaration in production Go. `type_alias` is a sibling child
//!    of `type_declaration` alongside `type_spec`, per the grammar.
//!
//! 2. **Interface methods are `method_elem`, not `method_declaration`.**
//!    `method_declaration` is reserved for top-level methods with
//!    receivers (`func (w *W) M() {}`). Interface members
//!    (`type R interface { F() }`) parse as `method_elem` nodes
//!    inside `interface_type`. Without this arm, `.go` files
//!    defining interfaces would extract the interface itself but
//!    none of its members — a retrieval blind spot for every
//!    contract-heavy Go codebase.
//!
//! ## Parent linkage
//!
//! Standard walker propagation handles `interface → method_elem`:
//! when `type_spec` emits an `Interface`, its descendants
//! (including the `method_elem` nodes two levels deep inside the
//! `interface_type` child) inherit `parent_idx` pointing at the
//! interface. Top-level `method_declaration` gets `parent_idx = None`
//! because Go's AST models methods as source-file-level declarations,
//! not as nested children of the receiver type — the receiver is
//! just a `parameter_list` on the method node.
//!
//! ## Out-of-scope / caveats
//!
//! - **Struct fields** (`field_declaration`): not emitted as symbols.
//!   Retrieval queries surface the containing struct; a future PR
//!   could add `Field` if demanded.
//! - **Package-level `var` declarations**: not emitted. Go-style
//!   package-level state is uncommon; `const` suffices for the
//!   retrieval surface v2.1-D ships.
//! - **Generic type parameters** (`type_parameter_declaration`):
//!   consumed structurally, not emitted as symbols — they are
//!   local to the declaration they parameterise.
//! - **Embedded interface references** inside `interface_type`
//!   (e.g. `interface { Drawable; Render() }`) are not emitted as
//!   their own symbols; the walker descends through them but no
//!   Method/Interface node is produced for the bare reference.

use super::common::{line_range, name_via_field, short_digest};
use super::ExtractedSymbol;
use azoth_core::retrieval::SymbolKind;
use tree_sitter::{Node, Parser, Tree};

/// Build a tree-sitter [`Parser`] pre-configured for Go 0.21. The
/// caller owns the instance and reuses it across every `.go` file in
/// a reindex pass (see `rust.rs` module docs for the reuse rationale).
pub fn go_parser() -> Result<Parser, super::ExtractError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_go::language())
        .map_err(|_| super::ExtractError::Language)?;
    Ok(parser)
}

/// Parse `src` with the caller-supplied Go parser and extract every
/// symbol the grammar recognises. The parser is expected to have Go
/// set as its language already (see [`go_parser`]).
pub fn extract_go(
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
/// Go doesn't need the `enclosing_container_is_class` flag Python and
/// TypeScript thread — method vs function distinction in Go comes
/// from NODE KIND (`method_declaration` vs `function_declaration`,
/// `method_elem` vs any function), not from context. Walker shape
/// therefore mirrors the simpler [`super::rust::walk`] rather than the
/// Python variant.
///
/// Iterative for the same stack-safety reason as every other grammar:
/// a 1 MiB `.go` file of `(` characters encodes ~1M nodes, well past
/// the 8 MiB Linux default thread stack at ~256 B/frame. Pre-order is
/// preserved by pushing children in reverse so pop order matches the
/// recursive descent shape.
fn walk(root: Node<'_>, bytes: &[u8], out: &mut Vec<ExtractedSymbol>) {
    let mut stack: Vec<(Node<'_>, Option<usize>)> = vec![(root, None)];
    // Single reused TreeCursor. `node.walk()` per iteration allocates
    // a fresh TSTreeCursor C struct; `cursor.reset(node)` avoids that
    // per-node allocation without changing the walk's O(N) complexity.
    // Pattern established by PR #20 round 7 (gemini MED on
    // `cf4c6ac`); carried through every subsequent grammar addition.
    let mut cursor = root.walk();
    while let Some((node, parent_idx)) = stack.pop() {
        // Classify node; a `const_spec` may emit multiple symbols
        // (one per identifier in its multi-valued `name` field), so
        // classification is split from emission.
        let emitted = classify_and_emit(node, bytes, parent_idx, out);

        // `next_parent` = the last symbol emitted from this node, or
        // the inherited parent if the node emitted nothing. Using the
        // last-emitted index rather than the first matters for
        // `const_spec` multi-name cases: we want children of the
        // const_spec (which currently are only leaf identifiers, but
        // a grammar update could change that) to see the most recent
        // emission. For non-multi-emit nodes the first-vs-last
        // distinction is moot.
        let next_parent = emitted.or(parent_idx);

        // TreeCursor forward walk + in-place reverse of the newly-
        // added stack tail. O(N) per parent, zero heap allocations.
        // See `python.rs::walk` for the (a)/(b)/(c) tradeoff
        // analysis behind this exact shape.
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

/// Classify a single node, emit zero-or-more symbols, return the
/// `Option<usize>` index to thread as `parent_idx` for descendants.
///
/// Most arms emit at most one symbol and return `Some(idx)` for it.
/// `const_spec` is the exception — its `name` field is grammar-marked
/// `multiple: true` (the parallel-declaration form `const A, B = 1, 2`
/// fits into a single const_spec node), so the arm iterates
/// `children_by_field_name("name")` and emits one `Const` per
/// identifier. Returns the LAST index so descendant nodes inside
/// the const_spec's `type` / `value` fields link to the most
/// recently emitted Const.
///
/// Using `children_by_field_name("name")` rather than a
/// `node.kind() == "identifier"` child scan is grammar-defensive:
/// if a future tree-sitter-go release introduces a new direct-child
/// identifier at `const_spec` scope (hypothetical but cheap to guard
/// against), the field-driven approach picks up only the intended
/// name identifiers.
fn classify_and_emit(
    node: Node<'_>,
    bytes: &[u8],
    parent_idx: Option<usize>,
    out: &mut Vec<ExtractedSymbol>,
) -> Option<usize> {
    match node.kind() {
        "package_clause" => {
            // REJECTED gemini MED 3123418508 (PR #23 review 1,
            // `398abd6`): "use `name_via_field` instead — the
            // grammar includes a `name` field." Verified
            // empirically against `tree-sitter-go 0.21.2`
            // (Cargo.lock): `name_via_field(node, "name", bytes)`
            // returns `None` because `package_clause.fields` is the
            // empty dict `{}` in this version's `node-types.json`.
            // The `package_identifier` is a plain positional child
            // under `children`, NOT a field-labeled one. Locked the
            // reject with a 30 s verification: swapping to the
            // suggested `name_via_field` patch made
            // `package_emits_symbol` fail with
            // "package symbol missing: []".
            //
            // Gemini's review body referenced "v0.21.0" — that
            // version MAY have carried a `name` field, but we're on
            // 0.21.2 per Cargo.lock and the grammar clearly shipped
            // `fields: {}` for this node. Cross-version grammar
            // drift is real; the node-types enumeration before
            // classifier fire (memory:
            // `pattern_tree_sitter_classifier_enumerate_node_types.md`)
            // was the right source of truth. Keeping the manual
            // children walk.
            let mut cur = node.walk();
            let name = node
                .children(&mut cur)
                .find(|c| c.kind() == "package_identifier")
                .and_then(|c| c.utf8_text(bytes).ok())
                .map(str::to_owned)?;
            Some(push_symbol(
                &name,
                SymbolKind::Package,
                &node,
                parent_idx,
                bytes,
                out,
            ))
        }
        "function_declaration" => name_via_field(&node, "name", bytes)
            .map(|n| push_symbol(&n, SymbolKind::Function, &node, parent_idx, bytes, out)),
        // Both `method_declaration` (top-level method with receiver)
        // and `method_elem` (interface member) classify as Method. The
        // plan's D4 draft only covered `method_declaration` and would
        // have silently dropped every interface method name — caught
        // by the node-types enumeration step before implementation.
        "method_declaration" | "method_elem" => name_via_field(&node, "name", bytes)
            .map(|n| push_symbol(&n, SymbolKind::Method, &node, parent_idx, bytes, out)),
        "type_spec" => {
            let name = name_via_field(&node, "name", bytes)?;
            let kind = match node.child_by_field_name("type").map(|c| c.kind()) {
                Some("struct_type") => SymbolKind::Struct,
                Some("interface_type") => SymbolKind::Interface,
                _ => SymbolKind::TypeAlias,
            };
            Some(push_symbol(&name, kind, &node, parent_idx, bytes, out))
        }
        // `type_alias` covers the Go 1.9+ `type X = Y` alias syntax,
        // a distinct grammar node from `type_spec`. Always classifies
        // as TypeAlias — the `= Y` marker carries the distinction the
        // type_spec arm has to discriminate via the `type` child.
        "type_alias" => name_via_field(&node, "name", bytes)
            .map(|n| push_symbol(&n, SymbolKind::TypeAlias, &node, parent_idx, bytes, out)),
        "const_spec" => {
            // `name` is `multiple: true` in the grammar. Iterate via
            // `children_by_field_name` so we pick up every identifier
            // in the parallel-declaration form (`const A, B = 1, 2`)
            // without accidentally matching identifiers in the
            // `value` expression subtree.
            //
            // **`.is_named()` filter is load-bearing.** tree-sitter-go's
            // grammar marks `const_spec.name.types` as
            // `[identifier(named), ","(named: false)]` with
            // `multiple: true`, so `children_by_field_name("name", ..)`
            // returns the comma separators AS WELL AS the identifier
            // nodes. Without `is_named()`, `utf8_text` reads ","
            // successfully and the classifier emits
            // `ExtractedSymbol { name: ",", kind: Const }` rows
            // between the real identifiers — surfaced in PR #23 R1
            // while verifying gemini's (incorrect) `package_clause`
            // suggestion: the throwaway patch spilled the full
            // extractor output, revealing the comma leak on an
            // adjacent site. Regression test:
            // `const_multi_name_spec` now asserts no punctuation-only
            // symbols leak.
            let mut cur = node.walk();
            let mut last_idx: Option<usize> = None;
            for child in node.children_by_field_name("name", &mut cur) {
                if !child.is_named() {
                    continue;
                }
                if let Ok(name) = child.utf8_text(bytes) {
                    let idx = push_symbol(name, SymbolKind::Const, &node, parent_idx, bytes, out);
                    last_idx = Some(idx);
                }
            }
            last_idx
        }
        _ => None,
    }
}

/// Materialise an [`ExtractedSymbol`] into the output vector and
/// return its index. Line range and digest are computed from the
/// full classified node (for `const_spec` that means every
/// emitted `Const` shares the spec's line range and body digest —
/// correct for the multi-name case where `const A, B = 1, 2` is a
/// single source-level declaration).
fn push_symbol(
    name: &str,
    kind: SymbolKind,
    node: &Node<'_>,
    parent_idx: Option<usize>,
    bytes: &[u8],
    out: &mut Vec<ExtractedSymbol>,
) -> usize {
    let (start_line, end_line) = line_range(node);
    out.push(ExtractedSymbol {
        name: name.to_string(),
        kind,
        start_line,
        end_line,
        parent_idx,
        digest: short_digest(node, bytes),
    });
    out.len() - 1
}
