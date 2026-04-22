//! PR 2.1-D — Go tree-sitter extractor tests.
//!
//! Locks the v2.1-D ship criteria: every grammar node kind the
//! classifier touches surfaces in output, interface members (grammar
//! node `method_elem`) classify as Method with parent linkage to the
//! enclosing interface `type_spec`, the Go 1.9+ `type X = Y` alias
//! syntax emits `TypeAlias` (distinct grammar node `type_alias`,
//! separate from `type_spec` on the discriminant), multi-name const
//! specs (`const A, B = 1, 2`) emit one `Const` per identifier,
//! <50 ms per-file extraction budget on the 457-LOC fixture, no panic
//! on malformed input.
//!
//! Node-types enumeration done before implementation — see
//! `src/code_graph/go.rs` module docstring for the
//! `type_alias` / `method_elem` finds that the v2.1 implementation
//! plan's D3/D4 drafts missed.

use azoth_core::retrieval::SymbolKind;
use azoth_repo::code_graph::{extract_go, go_parser, ExtractedSymbol};
use std::time::Instant;

fn extract(src: &str) -> Vec<ExtractedSymbol> {
    let mut p = go_parser().expect("parser");
    extract_go(&mut p, src).expect("extract")
}

#[test]
fn function_extraction() {
    let syms = extract("package main\nfunc Alpha() {}\n");
    assert!(
        syms.iter()
            .any(|s| s.name == "Alpha" && s.kind == SymbolKind::Function),
        "Alpha not extracted as Function: {syms:?}"
    );
}

#[test]
fn method_declaration_extracted() {
    // `method_declaration` (top-level method with receiver) is a Go
    // grammar node distinct from `method_elem` — both must classify as
    // Method. Parent linkage to the receiver type is out of scope
    // (methods live at top level in Go's AST, not nested under the
    // struct's type_spec).
    let src = "package main\ntype W struct{}\nfunc (w *W) M() {}\n";
    let syms = extract(src);
    assert!(
        syms.iter()
            .any(|x| x.name == "W" && x.kind == SymbolKind::Struct),
        "struct W missing: {syms:?}",
    );
    assert!(
        syms.iter()
            .any(|x| x.name == "M" && x.kind == SymbolKind::Method),
        "method M missing: {syms:?}",
    );
}

#[test]
fn interface_and_type_spec_alias() {
    // `type ID int` is a `type_spec` with a non-struct/non-interface
    // `type` field child — classifier falls back to TypeAlias.
    // Distinct grammar node from `type_alias` (Go 1.9+ `type X = Y`)
    // which gets its own test below.
    let src = "package main\ntype R interface { F() }\ntype ID int\n";
    let syms = extract(src);
    assert!(
        syms.iter()
            .any(|x| x.name == "R" && x.kind == SymbolKind::Interface),
        "interface R missing: {syms:?}",
    );
    assert!(
        syms.iter()
            .any(|x| x.name == "ID" && x.kind == SymbolKind::TypeAlias),
        "type ID int (type_spec flavor) not classified as TypeAlias: {syms:?}",
    );
}

#[test]
fn type_alias_go_1_9_syntax() {
    // Grammar node `type_alias` — the Go 1.9+ `type X = Y` alias
    // syntax. Distinct AST node from `type_spec` even though both
    // emit `SymbolKind::TypeAlias`. The v2.1 plan's D3 draft did
    // NOT cover this node kind, which would silently miss real
    // alias declarations in production code.
    let src = "package main\ntype MyInt = int\ntype StringMap = map[string]string\n";
    let syms = extract(src);
    assert!(
        syms.iter()
            .any(|x| x.name == "MyInt" && x.kind == SymbolKind::TypeAlias),
        "type_alias `MyInt = int` not emitted as TypeAlias: {syms:?}",
    );
    assert!(
        syms.iter()
            .any(|x| x.name == "StringMap" && x.kind == SymbolKind::TypeAlias),
        "type_alias `StringMap = map[string]string` not emitted: {syms:?}",
    );
}

#[test]
fn interface_method_elem_linked_to_interface() {
    // Interface members use grammar node `method_elem`, NOT
    // `method_declaration`. Classifier must handle both kinds. The
    // parent_idx MUST point at the enclosing interface's type_spec
    // emission because retrieval queries like
    // "methods on interface R" rely on parent linkage being
    // structurally correct. The v2.1 plan's D3/D4 drafts missed
    // `method_elem` entirely — silently dropping every interface
    // method name from the symbol index.
    let src =
        "package main\ntype Drawable interface {\n\tDraw() string\n\tBounds() (int, int)\n}\n";
    let syms = extract(src);
    let iface_idx = syms
        .iter()
        .position(|x| x.name == "Drawable" && x.kind == SymbolKind::Interface)
        .unwrap_or_else(|| panic!("interface Drawable missing: {syms:?}"));

    let draw = syms
        .iter()
        .find(|x| x.name == "Draw" && x.kind == SymbolKind::Method)
        .unwrap_or_else(|| panic!("interface method `Draw` missing: {syms:?}"));
    assert_eq!(
        draw.parent_idx,
        Some(iface_idx),
        "interface method `Draw` parent_idx={:?} should point at Drawable idx={iface_idx}: {syms:?}",
        draw.parent_idx,
    );

    let bounds = syms
        .iter()
        .find(|x| x.name == "Bounds" && x.kind == SymbolKind::Method)
        .unwrap_or_else(|| panic!("interface method `Bounds` missing: {syms:?}"));
    assert_eq!(
        bounds.parent_idx,
        Some(iface_idx),
        "interface method `Bounds` parent_idx={:?} should point at Drawable idx={iface_idx}: {syms:?}",
        bounds.parent_idx,
    );
}

#[test]
fn package_emits_symbol() {
    let syms = extract("package mypkg\n");
    assert!(
        syms.iter()
            .any(|x| x.name == "mypkg" && x.kind == SymbolKind::Package),
        "package symbol missing: {syms:?}",
    );
}

#[test]
fn const_single_declaration() {
    let syms = extract("package main\nconst A = 1\n");
    assert!(
        syms.iter()
            .any(|x| x.name == "A" && x.kind == SymbolKind::Const),
        "const A missing: {syms:?}",
    );
}

#[test]
fn const_block_declaration() {
    let syms = extract("package main\nconst (\n\tB = 2\n\tC = 3\n)\n");
    assert!(
        syms.iter()
            .any(|x| x.name == "B" && x.kind == SymbolKind::Const),
        "const B in block missing: {syms:?}",
    );
    assert!(
        syms.iter()
            .any(|x| x.name == "C" && x.kind == SymbolKind::Const),
        "const C in block missing: {syms:?}",
    );
}

#[test]
fn const_multi_name_spec() {
    // `const_spec.name` has `multiple: true` in the grammar — a
    // single const_spec can carry multiple identifier children for
    // the parallel-declaration form `const A, B = 1, 2`. The
    // classifier must iterate `children_by_field_name("name")`
    // rather than doing a kind=="identifier" child scan, so we
    // don't accidentally emit identifiers that appear in the
    // `value` subtree (e.g. `const X = someVar`).
    let src = "package main\nconst One, Two, Three = 1, 2, 3\n";
    let syms = extract(src);
    for name in ["One", "Two", "Three"] {
        assert!(
            syms.iter()
                .any(|x| x.name == name && x.kind == SymbolKind::Const),
            "multi-name const `{name}` missing: {syms:?}",
        );
    }
}

#[test]
fn malformed_no_panic() {
    // Adversarial input must not panic. tree-sitter-go's error
    // recovery produces SOME tree for every byte sequence; we do
    // not assert specific recoveries beyond "doesn't crash".
    let syms = extract("package main\nfunc ok() {}\n~~garbage~~\nfunc done() {}\n");
    // Soft assertion — error recovery MAY drop the surrounding
    // functions if the garbage token disrupts the enclosing
    // source_file. At minimum we must not panic.
    let _ = syms;
}

#[test]
fn perf_budget_500_loc() {
    let src = std::fs::read_to_string("tests/fixtures/go/sample.go").expect("fixture readable");
    assert!(
        src.len() > 500,
        "fixture too small ({} bytes) — perf budget test is meaningless on toy input",
        src.len(),
    );
    let t0 = Instant::now();
    let syms = extract(&src);
    let elapsed = t0.elapsed();
    assert!(
        elapsed.as_millis() < 50,
        "extraction budget exceeded: {elapsed:?} (limit 50 ms); {} symbols extracted",
        syms.len(),
    );
}

#[test]
fn fixture_surface_coverage() {
    // Guard against classifier regression on the full fixture: we
    // already know the node-kind set the walker should emit, so
    // floors keep a future narrowing change honest. Numbers come
    // from manual inspection of the fixture (function count, etc.)
    // intentionally loose — exact counts shift with pad helpers.
    let src = std::fs::read_to_string("tests/fixtures/go/sample.go").expect("fixture readable");
    let syms = extract(&src);

    let count_kind = |k: SymbolKind| syms.iter().filter(|s| s.kind == k).count();

    assert!(
        count_kind(SymbolKind::Package) >= 1,
        "expected ≥1 Package symbol in fixture: {syms:?}",
    );
    assert!(
        count_kind(SymbolKind::Function) >= 40,
        "expected ≥40 Function symbols in fixture, got {}",
        count_kind(SymbolKind::Function),
    );
    assert!(
        count_kind(SymbolKind::Method) >= 20,
        "expected ≥20 Method symbols (struct methods + interface method_elems), got {}",
        count_kind(SymbolKind::Method),
    );
    assert!(
        count_kind(SymbolKind::Struct) >= 5,
        "expected ≥5 Struct symbols, got {}",
        count_kind(SymbolKind::Struct),
    );
    assert!(
        count_kind(SymbolKind::Interface) >= 5,
        "expected ≥5 Interface symbols, got {}",
        count_kind(SymbolKind::Interface),
    );
    assert!(
        count_kind(SymbolKind::TypeAlias) >= 4,
        "expected ≥4 TypeAlias symbols (type_alias nodes + type_spec-non-struct-non-iface), got {}",
        count_kind(SymbolKind::TypeAlias),
    );
    assert!(
        count_kind(SymbolKind::Const) >= 15,
        "expected ≥15 Const symbols in fixture, got {}",
        count_kind(SymbolKind::Const),
    );
}
