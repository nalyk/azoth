//! Shared utilities for per-language tree-sitter walkers.
//!
//! Consolidated from `rust.rs` and `python.rs` where byte-identical
//! copies had diverged silently (gemini MED on PR #20 c160389). The
//! round-2 `short_digest` clamp fix had to be applied at both sites;
//! keeping one source of truth here ensures the next fix — and the
//! next grammar (PR 2.1-C TypeScript, PR 2.1-D Go) — inherits the
//! fix automatically.
//!
//! The walker itself stays per-language (tree-sitter node kinds are
//! grammar-specific), but every helper that operates on raw
//! `tree_sitter::Node` + source bytes lives here.

use sha2::{Digest, Sha256};
use tree_sitter::Node;

/// SHA-256 digest of the node's source bytes, truncated to 16 hex
/// chars.
///
/// Debug/forensic column — not a security boundary — but must survive
/// a rustc toolchain bump to be useful for cross-session diffs.
/// `std::collections::hash_map::DefaultHasher`'s algorithm is
/// explicitly unspecified across Rust versions (per the std docs), so
/// SHA-256 here for algorithmic stability. Truncating to 16 hex
/// chars keeps the column narrow while leaving 64 bits of collision
/// resistance — ample for a "did this body change" check.
///
/// Both indices are clamped to `bytes.len()` before slicing. Gemini
/// raised the panic risk on PR #20: on a tree-sitter state where
/// `start_byte > bytes.len()` (error recovery, truncated source
/// between parse and walk), the unclamped slice panics. The clamp is
/// pure defence — on well-formed trees tree-sitter guarantees
/// `start_byte <= end_byte <= bytes.len()` so the clamp is a no-op;
/// the fix only matters for pathological states.
pub(super) fn short_digest(node: &Node<'_>, bytes: &[u8]) -> String {
    let start = node.start_byte().min(bytes.len());
    let end = node.end_byte().min(bytes.len());
    let slice = &bytes[start..end];
    let mut h = Sha256::new();
    h.update(slice);
    let digest = h.finalize();
    hex::encode(&digest[..8])
}

/// 1-based line numbers (tree-sitter emits 0-based; we convert).
pub(super) fn line_range(node: &Node<'_>) -> (u32, u32) {
    let s = node.start_position().row;
    let e = node.end_position().row;
    ((s as u32).saturating_add(1), (e as u32).saturating_add(1))
}

/// Read a named field from `node` as UTF-8 text, returning `None` if
/// the field is missing or contains invalid UTF-8.
pub(super) fn name_via_field(node: &Node<'_>, field: &str, bytes: &[u8]) -> Option<String> {
    node.child_by_field_name(field)
        .and_then(|c| c.utf8_text(bytes).ok())
        .map(str::to_owned)
}
