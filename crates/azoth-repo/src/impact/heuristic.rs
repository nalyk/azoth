//! Shared substring predicates for the TDAD ecosystem selectors.
//!
//! All three selectors (`cargo`, `pytest`, `jest`) match changed-file
//! stems against test identifiers via substring. A naive `contains`
//! false-positives on any token whose prefix matches the stem — e.g.
//! a change in `auth.rs`/`auth.py`/`auth.ts` pulling every test
//! belonging to an unrelated `author` module / `author.test.ts` /
//! `test_author.py`. PR #25 R3 gemini flagged this on both jest and
//! pytest; cargo carries the same class bug and is swept here too
//! (sibling-audit discipline — see `feedback_audit_sibling_sites_on_class_bugs`
//! in auto-memory).
//!
//! The fix is a word-boundary guard: the match only counts when both
//! sides of the hit are either a string boundary or a non-alphanumeric
//! character. Rust's `char::is_alphanumeric` treats `_`, `.`, `:`, `/`
//! as non-alphanumeric, which is what we want:
//!
//! - `foo.test.ts` (jest convention) — `.` boundaries
//! - `test_foo.py` (pytest convention) — `_` and `.` boundaries
//! - `my_crate::foo::tests::alpha` (cargo libtest format) — `:` boundaries

/// True when `needle` appears in `haystack` with both sides bounded by
/// either the string edge or a non-alphanumeric character.
///
/// - Empty needle returns `false` (no meaningful match; prevents the
///   "every string contains the empty string" degeneracy the callers
///   already guard against by skipping empty stems, but asserted here
///   too to make the predicate self-contained).
/// - Iterates all occurrences via `str::match_indices` so the predicate
///   accepts later hits even when an earlier one fails the boundary check
///   (e.g. `foo` in `barfoo_bar.test.ts` — first hit at idx 3 has an
///   alphanumeric before-char but the trailing `_bar` after-char is
///   non-alphanumeric, so the match still resolves correctly).
pub(crate) fn word_boundary_contains(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    haystack.match_indices(needle).any(|(idx, _)| {
        let before_ok = haystack[..idx]
            .chars()
            .next_back()
            .map_or(true, |c| !c.is_alphanumeric());
        let after_start = idx + needle.len();
        let after_ok = haystack[after_start..]
            .chars()
            .next()
            .map_or(true, |c| !c.is_alphanumeric());
        before_ok && after_ok
    })
}

#[cfg(test)]
mod tests {
    use super::word_boundary_contains;

    #[test]
    fn accepts_match_at_both_string_boundaries() {
        assert!(word_boundary_contains("foo", "foo"));
    }

    #[test]
    fn accepts_match_with_dot_boundaries() {
        // jest canonical form `foo.test.ts`.
        assert!(word_boundary_contains("foo.test.ts", "foo"));
    }

    #[test]
    fn accepts_match_with_underscore_boundaries() {
        // pytest canonical form `test_foo.py`.
        assert!(word_boundary_contains("test_foo.py", "foo"));
    }

    #[test]
    fn accepts_match_with_colon_boundaries() {
        // cargo libtest canonical form `my_crate::foo::tests::alpha`.
        assert!(word_boundary_contains("my_crate::foo::tests::alpha", "foo"));
    }

    #[test]
    fn rejects_alphanumeric_suffix_prefix_collision() {
        // The class bug: `auth` vs `author.test.ts`.
        assert!(!word_boundary_contains("author.test.ts", "auth"));
    }

    #[test]
    fn rejects_alphanumeric_prefix_suffix_collision() {
        // Symmetric to above: `foo` vs `barfoo.test.ts`.
        assert!(!word_boundary_contains("barfoo.test.ts", "foo"));
    }

    #[test]
    fn rejects_embedded_token_with_alphanumeric_sides() {
        // `foo` fully embedded in `barfoobaz`.
        assert!(!word_boundary_contains("barfoobaz.test.ts", "foo"));
    }

    #[test]
    fn rejects_python_prefix_collision() {
        // `auth` vs `test_author.py` — pytest variant of the class bug.
        assert!(!word_boundary_contains("test_author.py", "auth"));
    }

    #[test]
    fn rejects_rust_module_prefix_collision() {
        // `auth` vs `my_crate::author::tests::foo` — cargo variant.
        assert!(!word_boundary_contains(
            "my_crate::author::tests::foo",
            "auth"
        ));
    }

    #[test]
    fn picks_up_later_hit_when_earlier_hit_fails_boundary() {
        // First occurrence at idx 0 has after-char `b` (alphanumeric) →
        // fails. Second occurrence at idx 4 is `_foo_` — both neighbours
        // non-alphanumeric → succeeds. The any() iteration must reach it.
        assert!(word_boundary_contains("foob_foo_bar.test", "foo"));
    }

    #[test]
    fn empty_needle_returns_false() {
        assert!(!word_boundary_contains("whatever", ""));
    }

    #[test]
    fn empty_haystack_returns_false() {
        assert!(!word_boundary_contains("", "foo"));
    }

    #[test]
    fn needle_equals_haystack_accepts() {
        assert!(word_boundary_contains("foo", "foo"));
    }

    #[test]
    fn needle_longer_than_haystack_rejects() {
        assert!(!word_boundary_contains("foo", "foobar"));
    }

    #[test]
    fn unicode_letter_neighbour_rejects() {
        // Unicode is_alphanumeric covers non-ASCII letters — `é` is
        // alphanumeric so the word-boundary check must reject.
        assert!(!word_boundary_contains("éfoo.test.ts", "foo"));
    }

    #[test]
    fn non_alphanumeric_unicode_neighbour_accepts() {
        // Em-dash U+2014 is non-alphanumeric punctuation; treat as boundary.
        assert!(word_boundary_contains("foo\u{2014}bar.ts", "foo"));
    }

    #[test]
    fn dotless_extension_still_matches_at_start() {
        // `foo` at start of a bare filename has the start-of-string
        // boundary on the left and the `.` boundary on the right.
        assert!(word_boundary_contains("foo.ts", "foo"));
    }

    #[test]
    fn full_filename_as_stem_matches() {
        // `foo_util` stem against `test_foo_util.py::test_case` — both
        // sides bounded by `_` / `.`.
        assert!(word_boundary_contains(
            "test_foo_util.py::test_case",
            "foo_util"
        ));
    }

    #[test]
    fn filename_stem_rejects_superstring() {
        // `foo` vs `test_foobar_util.py` — even though foo appears, the
        // `b` after-char blocks the match.
        assert!(!word_boundary_contains("test_foobar_util.py", "foo"));
    }
}
