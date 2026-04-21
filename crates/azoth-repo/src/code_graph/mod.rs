//! Symbol graph subsystem — tree-sitter extraction + SQLite storage.
//!
//! v2.0 shipped the Rust-only extractor. **v2.1** introduces the
//! `Language` enum + `detect_language` + `extract_for` dispatcher so
//! multi-grammar extraction routes through a single seam. Per-language
//! modules each expose `extract_<lang>(&mut Parser, &str)`; PRs B/C/D
//! add `python`, `typescript`, `go`.
//!
//! ## Why a dispatcher rather than a trait object
//!
//! The walker shape is grammar-specific (tree-sitter node kinds differ
//! per language). A trait object would force either a hand-rolled
//! vtable with identical walker logic per impl, or a `dyn` boundary
//! with parser lifetime drift. The pure `match` on `Language` stays
//! allocation-free and makes the call-site obvious to the reader.

pub mod index;
pub mod rust;

pub use index::{replace_symbols_for_path, SqliteSymbolIndex, SymbolWriter};
pub use rust::{extract_rust, rust_parser, ExtractError, ExtractedSymbol};

use std::path::Path;

/// Languages with a tree-sitter grammar wired in. Additive variant —
/// callers must `match` exhaustively so new grammars surface as
/// compile errors in every dispatcher.
///
/// Not `Serialize`/`Deserialize` by design — JSONL carries the
/// language as a string tag (matching `documents.language`) rather
/// than this enum; keeping the wire layer loose lets unknown languages
/// round-trip without `Language` churn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Python,
    TypeScript,
    Go,
}

impl Language {
    /// Stable tag persisted into `documents.language` and
    /// `symbols.language`. Do NOT change the strings without a
    /// migration — they are the durable surface.
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::TypeScript => "typescript",
            Language::Go => "go",
        }
    }

    /// Inverse of `as_str` — lets the indexer widen a
    /// `documents.language` column value back to a `Language` for
    /// dispatch. Returns `None` for tags outside the grammar-wired
    /// set (markdown, toml, javascript, etc.).
    pub fn from_wire(tag: &str) -> Option<Self> {
        match tag {
            "rust" => Some(Language::Rust),
            "python" => Some(Language::Python),
            "typescript" => Some(Language::TypeScript),
            "go" => Some(Language::Go),
            _ => None,
        }
    }
}

/// Extension-driven language detector. Returns `None` for files
/// outside the v2.1 grammar scope — JavaScript (`.js`/`.jsx`/`.mjs`/
/// `.cjs`) is explicitly not included (see v2.1 spec §Architecture).
/// Extension matching is case-sensitive, mirroring the v2.0 indexer
/// behaviour; case-insensitive matching would surprise real repos
/// that distinguish `.js` from `.JS` on case-sensitive filesystems.
pub fn detect_language(path: &Path) -> Option<Language> {
    let ext = path.extension().and_then(|s| s.to_str())?;
    match ext {
        "rs" => Some(Language::Rust),
        "py" => Some(Language::Python),
        "ts" | "tsx" => Some(Language::TypeScript),
        "go" => Some(Language::Go),
        _ => None,
    }
}

/// Dispatch entry point. Each new grammar adds one arm. Returns
/// `Err(ExtractError::UnsupportedLanguage(lang))` for languages
/// without a grammar wired in — distinct from
/// `ExtractError::Language` (tree-sitter ABI failure) so callers can
/// treat the two cases differently. The indexer routes
/// `UnsupportedLanguage` to a silent skip (expected state until PRs
/// 2.1-B/C/D land) while `Language` remains a log-worthy failure.
pub fn extract_for(
    lang: Language,
    parser: &mut tree_sitter::Parser,
    src: &str,
) -> Result<Vec<ExtractedSymbol>, ExtractError> {
    match lang {
        Language::Rust => extract_rust(parser, src),
        // PRs 2.1-B / 2.1-C / 2.1-D replace each arm with the real
        // extractor once the grammar lands.
        Language::Python | Language::TypeScript | Language::Go => {
            Err(ExtractError::UnsupportedLanguage(lang))
        }
    }
}

/// Path-aware parser factory. TypeScript routes `.tsx` paths through
/// the `LANGUAGE_TSX` grammar and everything else through
/// `LANGUAGE_TYPESCRIPT`; Python/Rust/Go have a single grammar so
/// the path is unused for them. Kept as a separate function from
/// `extract_for` because the indexer caches parsers per-language and
/// paying the `set_language` cost per file would blow the 50 ms
/// per-file budget spelled out in the v2.1 ship criteria.
///
/// v2.0-level parity: for `Language::Rust` this is identical to
/// calling `rust_parser()` directly; the signature accepts `path`
/// uniformly so v2.1 callers don't need a special-case.
pub fn parser_for(lang: Language, _path: &Path) -> Result<tree_sitter::Parser, ExtractError> {
    match lang {
        Language::Rust => rust_parser(),
        // PRs 2.1-B / C / D wire their constructors here. TypeScript
        // (PR-C) will use `_path` to choose between TS and TSX parser
        // constructors. Until each grammar lands, callers get
        // `UnsupportedLanguage` (deliberate) rather than `Language`
        // (ABI failure).
        Language::Python | Language::TypeScript | Language::Go => {
            Err(ExtractError::UnsupportedLanguage(lang))
        }
    }
}

/// Cache-discriminator for `parser_for`. `Language` alone is NOT a
/// sufficient cache key because TypeScript is **path-sensitive**:
/// `.ts` routes to `LANGUAGE_TYPESCRIPT`, `.tsx` to `LANGUAGE_TSX`
/// (PR 2.1-C). Caching by `Language` means the first TS file touched
/// decides the flavor and every subsequent TS file reuses that
/// parser — mixed `.ts`/`.tsx` repos then either panic (grammar
/// mismatch) or silently drop symbols. See codex P2 on PR #19.
///
/// The split from `Language` keeps the public "language concept"
/// surface stable (Rust / Python / TypeScript / Go) while the
/// **parser choice** surface — which is what the HashMap keys on —
/// evolves independently. New path-sensitive languages (e.g. Python
/// stubs, `.pyi`) would add a `ParserKey` variant without churning
/// `Language`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParserKey {
    Rust,
    Python,
    TypeScriptTs,
    TypeScriptTsx,
    Go,
}

/// Compute the `ParserKey` that `parser_for(lang, path)` would yield.
/// Single source of truth for path-sensitive discrimination — the
/// indexer uses this to key its parser cache so mixed `.ts`/`.tsx`
/// repos keep two distinct parsers live after PR 2.1-C wires the
/// TypeScript grammar. Inputs that `detect_language` would reject
/// never reach this function; the path extension is only consulted
/// for the one language that actually needs it.
pub fn parser_key(lang: Language, path: &Path) -> ParserKey {
    match lang {
        Language::Rust => ParserKey::Rust,
        Language::Python => ParserKey::Python,
        Language::TypeScript => match path.extension().and_then(|s| s.to_str()) {
            Some("tsx") => ParserKey::TypeScriptTsx,
            Some("ts") => ParserKey::TypeScriptTs,
            // `detect_language` and `parser_key` are a **paired
            // invariant**: every extension admitted by the former's
            // TypeScript arm must have a conscious landing arm here.
            // Today the former only admits `.ts`/`.tsx`, so reaching
            // this branch is a violation of the paired invariant —
            // not a graceful-degradation opportunity. `unreachable!()`
            // (gemini MED on PR #19 b1ddfeb, preferred over the
            // round-4 `debug_assert!(false, …)` because the branch is
            // genuinely unreachable under the current contract, and
            // the idiomatic macro signals that to both reader and
            // compiler). Widening `detect_language` to `.mts`/`.cts`
            // in a future PR MUST widen this match in the same
            // commit; CI tests that exercise the new extension will
            // hit this panic immediately.
            other => unreachable!(
                "parser_key: unhandled TypeScript extension {other:?} — \
                 widen detect_language and parser_key together"
            ),
        },
        Language::Go => ParserKey::Go,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn language_wire_tags_round_trip() {
        for lang in [
            Language::Rust,
            Language::Python,
            Language::TypeScript,
            Language::Go,
        ] {
            let s = lang.as_str();
            assert_eq!(Language::from_wire(s), Some(lang), "tag {s}");
        }
        assert_eq!(Language::from_wire("markdown"), None);
        assert_eq!(Language::from_wire(""), None);
    }

    #[test]
    fn detect_language_routes_twenty_fixtures() {
        // PR 2.1-A ship criterion: 20 path fixtures across 4 languages.
        let cases: &[(&str, Option<Language>)] = &[
            // Rust (5)
            ("src/foo.rs", Some(Language::Rust)),
            ("crates/a/src/lib.rs", Some(Language::Rust)),
            ("tests/e2e.rs", Some(Language::Rust)),
            ("nested/deep/module.rs", Some(Language::Rust)),
            ("one_char.rs", Some(Language::Rust)),
            // Python (4)
            ("lib/bar.py", Some(Language::Python)),
            ("src/pkg/__init__.py", Some(Language::Python)),
            ("tests/test_alpha.py", Some(Language::Python)),
            ("nested.module.py", Some(Language::Python)),
            // TypeScript (5, including .tsx)
            ("app/x.ts", Some(Language::TypeScript)),
            ("src/components/Button.tsx", Some(Language::TypeScript)),
            ("lib/types.d.ts", Some(Language::TypeScript)),
            ("deeply/nested/file.tsx", Some(Language::TypeScript)),
            ("a.ts", Some(Language::TypeScript)),
            // Go (3)
            ("cmd/main.go", Some(Language::Go)),
            ("pkg/http/server.go", Some(Language::Go)),
            ("tests/integration_test.go", Some(Language::Go)),
            // Out-of-scope (3)
            ("docs/readme.md", None),
            ("Cargo.toml", None),
            ("src/component.jsx", None),
        ];
        assert_eq!(cases.len(), 20, "20 fixtures required");
        for (path, want) in cases {
            let got = detect_language(Path::new(path));
            assert_eq!(got, *want, "path={path}");
        }
    }

    #[test]
    fn detect_language_no_extension() {
        assert_eq!(detect_language(&PathBuf::from("CHANGELOG")), None);
        assert_eq!(detect_language(&PathBuf::from("no_ext")), None);
    }

    #[test]
    fn detect_language_case_sensitive() {
        // Mirrors the v2.0 `indexer::detect_language` behaviour —
        // uppercase `.RS` is not treated as Rust. Documented so a
        // future sweep doesn't introduce case-insensitive matching
        // without consciousness of the consequences.
        assert_eq!(detect_language(&PathBuf::from("x.RS")), None);
        assert_eq!(detect_language(&PathBuf::from("x.PY")), None);
        assert_eq!(detect_language(&PathBuf::from("x.TS")), None);
    }

    #[test]
    fn extract_for_rust_dispatches_to_rust_extractor() {
        let mut parser = rust_parser().expect("parser");
        let syms = extract_for(Language::Rust, &mut parser, "fn alpha() {}\n").unwrap();
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "alpha");
    }

    #[test]
    fn extract_for_pending_languages_errors() {
        // Pre-B/C/D guard: Python/TS/Go return
        // `ExtractError::UnsupportedLanguage(<lang>)`, not
        // `ExtractError::Language` (which is reserved for tree-sitter
        // ABI failure on an already-wired grammar). When the grammar
        // lands, the corresponding arm is removed in that PR.
        let mut parser = rust_parser().unwrap();
        for lang in [Language::Python, Language::TypeScript, Language::Go] {
            match extract_for(lang, &mut parser, "") {
                Err(ExtractError::UnsupportedLanguage(got)) => assert_eq!(got, lang),
                other => panic!("lang={lang:?}: expected UnsupportedLanguage, got {other:?}"),
            }
        }
    }

    #[test]
    fn parser_for_rust_uniform_path() {
        let _ = parser_for(Language::Rust, Path::new("whatever.rs")).expect("rust parser");
    }

    #[test]
    fn parser_for_pending_languages_errors() {
        // Sibling to `extract_for_pending_languages_errors`. Ensures
        // the parser factory and the extractor share the same error
        // taxonomy so the indexer can treat the two call sites
        // symmetrically. `tree_sitter::Parser` doesn't implement
        // `Debug`, so `{other:?}` on the whole `Result` won't compile;
        // we match the branches directly and name the unexpected Ok
        // case in its own arm.
        for lang in [Language::Python, Language::TypeScript, Language::Go] {
            match parser_for(lang, Path::new("x.any")) {
                Err(ExtractError::UnsupportedLanguage(got)) => assert_eq!(got, lang),
                Ok(_) => panic!("lang={lang:?}: expected UnsupportedLanguage, got Ok"),
                Err(other) => panic!("lang={lang:?}: expected UnsupportedLanguage, got {other:?}"),
            }
        }
    }

    #[test]
    fn parser_key_typescript_discriminates_tsx() {
        // Codex P2 on PR #19: mixed TS/TSX repos must keep two
        // distinct parser cache slots live. `.ts` and `.tsx` route to
        // different `ParserKey` variants so the indexer's
        // `HashMap<ParserKey, Parser>` holds a parser per flavor once
        // PR 2.1-C wires the TypeScript grammar.
        assert_eq!(
            parser_key(Language::TypeScript, Path::new("app/x.ts")),
            ParserKey::TypeScriptTs,
        );
        assert_eq!(
            parser_key(Language::TypeScript, Path::new("lib/types.d.ts")),
            ParserKey::TypeScriptTs,
        );
        assert_eq!(
            parser_key(Language::TypeScript, Path::new("app/Button.tsx")),
            ParserKey::TypeScriptTsx,
        );
        assert_ne!(
            parser_key(Language::TypeScript, Path::new("a.ts")),
            parser_key(Language::TypeScript, Path::new("a.tsx")),
            "TS and TSX must hash to distinct cache slots",
        );
    }

    #[test]
    fn parser_key_non_typescript_ignores_path() {
        // Rust/Python/Go have a single grammar per language, so the
        // path extension is irrelevant to the cache slot. Locks the
        // invariant so future additions of `.pyi` or similar are a
        // conscious `ParserKey` variant add, not a silent drift.
        assert_eq!(
            parser_key(Language::Rust, Path::new("src/foo.rs")),
            ParserKey::Rust,
        );
        assert_eq!(
            parser_key(Language::Rust, Path::new("whatever.unrelated")),
            ParserKey::Rust,
        );
        assert_eq!(
            parser_key(Language::Python, Path::new("a.py")),
            ParserKey::Python,
        );
        assert_eq!(
            parser_key(Language::Python, Path::new("a.pyi")),
            ParserKey::Python,
            "stub files share the cache slot until a parser flavor is added",
        );
        assert_eq!(parser_key(Language::Go, Path::new("a.go")), ParserKey::Go,);
    }
}
