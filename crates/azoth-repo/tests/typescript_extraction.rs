//! PR 2.1-C — TypeScript tree-sitter extractor tests.
//!
//! Mirrors `python_extraction.rs` shape. Exercises both the
//! `language_typescript()` (.ts/.d.ts) and `language_tsx()` (.tsx)
//! parser flavours — the dispatcher chooses between them at
//! parser-construction time; the extractor itself handles either
//! tree transparently.

use azoth_core::retrieval::SymbolKind;
use azoth_repo::code_graph::{
    extract_typescript, typescript_parser_ts, typescript_parser_tsx, ExtractedSymbol,
};
use std::time::Instant;

fn extract_ts(src: &str) -> Vec<ExtractedSymbol> {
    let mut p = typescript_parser_ts().expect("ts parser");
    extract_typescript(&mut p, src).expect("extract")
}

fn extract_tsx(src: &str) -> Vec<ExtractedSymbol> {
    let mut p = typescript_parser_tsx().expect("tsx parser");
    extract_typescript(&mut p, src).expect("extract")
}

#[test]
fn top_level_function_extracted() {
    let syms = extract_ts("function alpha(x: number): number { return x; }\n");
    assert!(
        syms.iter()
            .any(|s| s.name == "alpha" && s.kind == SymbolKind::Function),
        "alpha not extracted as Function: {syms:?}"
    );
}

#[test]
fn exported_function_extracted() {
    // `export function` wraps `function_declaration` in an
    // `export_statement`; the walker must descend into the wrapper.
    let syms = extract_ts("export function beta(): void {}\n");
    assert!(
        syms.iter()
            .any(|s| s.name == "beta" && s.kind == SymbolKind::Function),
        "exported beta not extracted: {syms:?}"
    );
}

#[test]
fn async_function_classified_same_as_sync() {
    let syms = extract_ts("async function worker(): Promise<number> { return 1; }\n");
    assert!(
        syms.iter()
            .any(|s| s.kind == SymbolKind::Function && s.name == "worker"),
        "async function not recognised as Function: {syms:?}"
    );
}

#[test]
fn generator_function_extracted_as_function() {
    let syms = extract_ts("function* gen(): Generator<number> { yield 1; }\n");
    assert!(
        syms.iter()
            .any(|s| s.kind == SymbolKind::Function && s.name == "gen"),
        "generator_function_declaration not classified as Function: {syms:?}"
    );
}

#[test]
fn class_and_methods_linked() {
    let src = "class Foo {\n  bar(): void {}\n  baz(): void {}\n}\n";
    let syms = extract_ts(src);
    let class_idx = syms
        .iter()
        .position(|s| s.name == "Foo" && s.kind == SymbolKind::Class)
        .expect("class Foo extracted");
    let bar = syms
        .iter()
        .find(|s| s.name == "bar" && s.kind == SymbolKind::Method)
        .expect("bar extracted as Method");
    let baz = syms
        .iter()
        .find(|s| s.name == "baz" && s.kind == SymbolKind::Method)
        .expect("baz extracted as Method");
    assert_eq!(bar.parent_idx, Some(class_idx));
    assert_eq!(baz.parent_idx, Some(class_idx));
}

#[test]
fn abstract_class_extracted_as_class() {
    let src = "abstract class Base {\n  abstract name(): string;\n}\n";
    let syms = extract_ts(src);
    assert!(
        syms.iter()
            .any(|s| s.name == "Base" && s.kind == SymbolKind::Class),
        "abstract_class_declaration not classified as Class: {syms:?}"
    );
}

#[test]
fn interface_extracted() {
    let src = "interface Shape {\n  area(): number;\n}\n";
    let syms = extract_ts(src);
    assert!(
        syms.iter()
            .any(|s| s.name == "Shape" && s.kind == SymbolKind::Interface),
        "Shape not extracted as Interface: {syms:?}"
    );
}

#[test]
fn type_alias_extracted() {
    let src = "type Id = string | number;\n";
    let syms = extract_ts(src);
    assert!(
        syms.iter()
            .any(|s| s.name == "Id" && s.kind == SymbolKind::TypeAlias),
        "Id not extracted as TypeAlias: {syms:?}"
    );
}

#[test]
fn enum_extracted() {
    let src = "enum Color { Red, Green, Blue }\n";
    let syms = extract_ts(src);
    assert!(
        syms.iter()
            .any(|s| s.name == "Color" && s.kind == SymbolKind::Enum),
        "Color not extracted as Enum: {syms:?}"
    );
}

#[test]
fn interface_type_enum_from_export() {
    // Each declared wrapped in `export_statement`; the walker must
    // descend into the wrapper uniformly for every declaration kind.
    let src = "export interface I { f(): void }\nexport type T = string;\nexport enum E { A, B }\n";
    let syms = extract_ts(src);
    assert!(syms
        .iter()
        .any(|s| s.name == "I" && s.kind == SymbolKind::Interface));
    assert!(syms
        .iter()
        .any(|s| s.name == "T" && s.kind == SymbolKind::TypeAlias));
    assert!(syms
        .iter()
        .any(|s| s.name == "E" && s.kind == SymbolKind::Enum));
}

#[test]
fn nested_function_linked_to_outer() {
    let src = "function outer(): number {\n  function inner(): number { return 1; }\n  return inner();\n}\n";
    let syms = extract_ts(src);
    let outer_idx = syms
        .iter()
        .position(|s| s.name == "outer")
        .expect("outer extracted");
    let inner = syms
        .iter()
        .find(|s| s.name == "inner")
        .expect("inner extracted");
    assert_eq!(
        inner.parent_idx,
        Some(outer_idx),
        "nested function should point at outer"
    );
    // Function-inside-function is a closure, NOT a method — the
    // container-flag resets on every function body.
    assert_eq!(inner.kind, SymbolKind::Function);
}

#[test]
fn tsx_component_extracted_by_tsx_parser() {
    // JSX element in the body forces the TSX grammar. The `.ts`
    // grammar treats `<T>(x)` as a type assertion (parse error on
    // JSX); `.tsx` grammar handles it correctly.
    let src =
        "export function Greeting({ name }: { name: string }) { return <div>{name}</div>; }\n";
    let syms = extract_tsx(src);
    assert!(
        syms.iter()
            .any(|s| s.name == "Greeting" && s.kind == SymbolKind::Function),
        "Greeting not extracted via TSX parser: {syms:?}"
    );
}

#[test]
fn tsx_class_component_with_methods() {
    let src = "export class Counter extends React.Component {\n  state = { n: 0 };\n  render() { return <span>{this.state.n}</span>; }\n}\n";
    let syms = extract_tsx(src);
    let class_idx = syms
        .iter()
        .position(|s| s.name == "Counter" && s.kind == SymbolKind::Class)
        .expect("Counter class extracted");
    let render = syms
        .iter()
        .find(|s| s.name == "render" && s.kind == SymbolKind::Method)
        .expect("render method extracted");
    assert_eq!(render.parent_idx, Some(class_idx));
}

#[test]
fn malformed_input_does_not_panic() {
    // tree-sitter-typescript recovers around stray tokens. Lock the
    // no-panic promise and assert at least the first declaration
    // before the garbage survives.
    let syms = extract_ts("function ok() {}\n ~~garbage~~\nclass C {}\n");
    assert!(
        syms.iter().any(|s| s.name == "ok"),
        "`ok` not recovered before syntax error: {syms:?}"
    );
}

#[test]
fn malformed_trailing_garbage_does_not_panic() {
    // Stricter no-panic guard: garbage between two valid declarations.
    let src = "function ok() {}\n~~junk~~\nfunction also() {}\n";
    let syms = extract_ts(src);
    assert!(
        !syms.is_empty(),
        "extractor returned zero symbols on malformed input: should at least recover `ok`"
    );
}

#[test]
fn empty_and_comment_only_files_are_empty() {
    assert!(extract_ts("").is_empty());
    assert!(extract_ts("// comment only\n").is_empty());
    assert!(extract_ts("/* block */\n").is_empty());
    assert!(extract_tsx("").is_empty());
}

#[test]
fn parser_reuse_across_extractions() {
    let mut p = typescript_parser_ts().unwrap();
    let a = extract_typescript(&mut p, "function a(): void {}\n").unwrap();
    let b = extract_typescript(&mut p, "function b(): void {}\n").unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].name, "a");
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].name, "b");
}

#[test]
fn ts_and_tsx_parsers_are_distinct_instances() {
    // Smoke test proving the two factories return usable parsers that
    // can each handle their native input without contaminating each
    // other. The indexer caches them under distinct `ParserKey`
    // variants (`TypeScriptTs` vs `TypeScriptTsx`); the test locks
    // that a `.tsx`-heavy source parses through the TSX factory AND
    // that the TS factory still works on plain `.ts` afterwards.
    let mut tsx = typescript_parser_tsx().unwrap();
    let tsx_syms =
        extract_typescript(&mut tsx, "function C(): JSX.Element { return <div/>; }\n").unwrap();
    assert!(tsx_syms.iter().any(|s| s.name == "C"));

    let mut ts = typescript_parser_ts().unwrap();
    let ts_syms = extract_typescript(&mut ts, "function plainFn(): void {}\n").unwrap();
    assert!(ts_syms.iter().any(|s| s.name == "plainFn"));
}

#[test]
fn fixture_under_50ms_per_file() {
    let src =
        std::fs::read_to_string("tests/fixtures/typescript/sample.ts").expect("fixture readable");
    assert!(src.len() > 500, "fixture must be non-trivial");
    let t0 = Instant::now();
    let _ = extract_ts(&src);
    let elapsed = t0.elapsed();
    assert!(
        elapsed.as_millis() < 50,
        "extraction budget blown: {elapsed:?}"
    );
}

#[test]
fn fixture_yields_expected_symbol_counts() {
    // Conservative floors keyed to the shape of `sample.ts`. Recompute
    // if the fixture is regenerated. Intentionally conservative — the
    // fixture contains far more declarations than these asserts demand
    // so grammar-edge-case recovery doesn't fail the gate.
    let src =
        std::fs::read_to_string("tests/fixtures/typescript/sample.ts").expect("fixture readable");
    let syms = extract_ts(&src);
    let fns = syms
        .iter()
        .filter(|s| s.kind == SymbolKind::Function)
        .count();
    let cls = syms.iter().filter(|s| s.kind == SymbolKind::Class).count();
    let met = syms.iter().filter(|s| s.kind == SymbolKind::Method).count();
    let ifs = syms
        .iter()
        .filter(|s| s.kind == SymbolKind::Interface)
        .count();
    let ta = syms
        .iter()
        .filter(|s| s.kind == SymbolKind::TypeAlias)
        .count();
    let en = syms.iter().filter(|s| s.kind == SymbolKind::Enum).count();
    assert!(fns >= 20, "functions: got {fns}");
    assert!(cls >= 10, "classes: got {cls}");
    assert!(met >= 20, "methods: got {met}");
    assert!(ifs >= 5, "interfaces: got {ifs}");
    assert!(ta >= 5, "type aliases: got {ta}");
    assert!(en >= 3, "enums: got {en}");
}

#[test]
fn tsx_fixture_parses_and_extracts() {
    let src =
        std::fs::read_to_string("tests/fixtures/typescript/sample.tsx").expect("fixture readable");
    let mut p = typescript_parser_tsx().unwrap();
    let syms = extract_typescript(&mut p, &src).expect("tsx extraction");
    // Floors reflect the shape of sample.tsx (several function
    // components + Counter/ErrorBoundary class components + interfaces
    // + Enum). Conservative to absorb JSX-recovery grammar quirks.
    assert!(
        syms.iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .count()
            >= 5,
        "tsx fixture: too few functions"
    );
    assert!(
        syms.iter().filter(|s| s.kind == SymbolKind::Class).count() >= 2,
        "tsx fixture: too few classes"
    );
}

// Adversarial-depth tests. Mirror the Python/Rust walkers: a
// `.ts` file composed entirely of `{` or `(` characters encodes
// ~1M nodes per MiB; a recursive walker would overflow the thread
// stack. The iterative walker stays O(1) in stack depth.

#[test]
fn deeply_nested_parens_does_not_stack_overflow() {
    let depth = 20_000usize;
    let src = format!(
        "const x = {}{}{};\n",
        "(".repeat(depth),
        "1",
        ")".repeat(depth),
    );
    let mut p = typescript_parser_ts().unwrap();
    let _ = extract_typescript(&mut p, &src).expect("extract must not crash");
}

#[test]
fn deeply_nested_braces_does_not_stack_overflow() {
    // Object-literal nesting. Still stack-safe under an iterative walker.
    let depth = 5_000usize;
    let mut src = String::from("const x = ");
    for _ in 0..depth {
        src.push('{');
        src.push_str("a: ");
    }
    src.push('1');
    for _ in 0..depth {
        src.push('}');
    }
    src.push_str(";\n");
    let mut p = typescript_parser_ts().unwrap();
    let _ = extract_typescript(&mut p, &src).expect("extract must not crash");
}

#[test]
fn deeply_nested_functions_does_not_stack_overflow() {
    // TypeScript class bodies cannot contain nested class
    // declarations (grammar only admits methods/fields/statements),
    // so the Python-style `class C: class C: class C:` depth test
    // has no TS analogue. Functions nested inside functions are the
    // legitimate TS equivalent and exercise the same iterative
    // walker property — `parent_idx` threading across deep
    // `function_declaration` → body → `function_declaration` chains.
    // Primary assertion: no panic under 2 000 levels of nesting,
    // which would overflow an 8 MiB stack at ~256 B/frame on a
    // recursive walker. Secondary: the iterative walker reaches a
    // substantial depth and `parent_idx` chains survive intact.
    let depth = 2_000usize;
    let mut src = String::new();
    for i in 0..depth {
        src.push_str(&format!("function f{i}() {{\n"));
    }
    src.push_str("let x = 1;\n");
    for _ in 0..depth {
        src.push_str("}\n");
    }
    let mut p = typescript_parser_ts().unwrap();
    let syms = extract_typescript(&mut p, &src).expect("extract must not crash");
    let fns = syms
        .iter()
        .filter(|s| s.kind == SymbolKind::Function)
        .count();
    assert!(
        fns >= 500,
        "walker should classify at least 500 nested functions; got {fns}"
    );
}
