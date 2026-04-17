; Sprint 2 — tree-sitter-rust 0.21 symbol queries.
;
; This file is kept as reference documentation of the grammar node
; shapes the extractor cares about. The actual extraction uses a
; recursive walk (see `code_graph::rust`) because tree-sitter queries
; don't give us the parent-pointer tracking we need for SQL FK links
; (method → impl, variant → enum). Queries remain handy for one-off
; diagnostics: `tree-sitter query` against a parsed file uses them.

; Top-level Rust constructs extracted as Symbols.
;
; Field names below map to tree-sitter-rust 0.21 grammar.json — these
; are the stable node shapes the extractor walks. ABI is pinned by the
; workspace Cargo.toml (tree-sitter 0.22 + tree-sitter-rust 0.21).

(function_item name: (identifier) @name) @function

(struct_item name: (type_identifier) @name) @struct

(enum_item name: (type_identifier) @name) @enum

(enum_variant name: (identifier) @name) @enum_variant

(trait_item name: (type_identifier) @name) @trait

; `impl_item` has no `name` field — we use the `type` field's text
; (what's being implemented on) as the symbol's name.
(impl_item type: (_) @name) @impl

(mod_item name: (identifier) @name) @module

(const_item name: (identifier) @name) @const
