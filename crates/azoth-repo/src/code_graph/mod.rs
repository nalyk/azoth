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
        // `Debug`, so we can't `{other:?}` the whole Result — drop
        // the Ok-payload to an `()` marker before any panic message.
        for lang in [Language::Python, Language::TypeScript, Language::Go] {
            let err = parser_for(lang, Path::new("x.any"))
                .map(|_parser_ok| ())
                .expect_err("expected UnsupportedLanguage");
            match err {
                ExtractError::UnsupportedLanguage(got) => assert_eq!(got, lang),
                other => panic!("lang={lang:?}: expected UnsupportedLanguage, got {other:?}"),
            }
        }
    }
}
