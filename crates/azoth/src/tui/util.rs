//! Shared utilities for the TUI render path.

/// Truncate `s` to at most `limit` Unicode scalar values, replacing the
/// dropped tail with a single `…`. When `s` already fits, returns the
/// original unchanged. Earlier code had four byte-identical copies of
/// this in `card.rs`, `inspector.rs`, `render.rs`, and `sheet.rs`;
/// gemini round-14 MED flagged the duplication.
pub fn truncate(s: &str, limit: usize) -> std::borrow::Cow<'_, str> {
    // Returns Cow so that strings already within the budget pass
    // through with zero allocation. Earlier signature was -> String,
    // which forced an allocation on every call even when the input
    // didn't need truncation (the common case in the TUI render path
    // where most labels are already short). Short-circuit on length
    // check stays so we don't walk huge strings.
    if s.chars().take(limit.saturating_add(1)).count() <= limit {
        std::borrow::Cow::Borrowed(s)
    } else {
        let mut t: String = s.chars().take(limit.saturating_sub(1)).collect();
        t.push('…');
        std::borrow::Cow::Owned(t)
    }
}
