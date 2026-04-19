//! Shared utilities for the TUI render path.

/// Truncate `s` to at most `limit` Unicode scalar values, replacing the
/// dropped tail with a single `…`. When `s` already fits, returns the
/// original unchanged. Earlier code had four byte-identical copies of
/// this in `card.rs`, `inspector.rs`, `render.rs`, and `sheet.rs`;
/// gemini round-14 MED flagged the duplication.
pub fn truncate(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(limit.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}
