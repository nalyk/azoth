//! TDAD test impact selection schemas.
//!
//! A `Diff` is the input — a list of changed file paths relative to the
//! repo root. A `TestPlan` is the output — an ordered list of `TestId`s
//! to run, each with an aligned rationale describing why the selector
//! picked it. Both types are plain serde POD; the behavioural trait
//! lives in `crate::impact::ImpactSelector`.
//!
//! Invariants:
//! - `TestId` is the selector-visible identifier. For
//!   `CargoTestImpact` this is the `package::path::test_fn` string
//!   emitted by `cargo test --list --format json`; for future
//!   per-ecosystem selectors (pytest, jest, go test) it is whatever
//!   the native runner uses to scope a single test.
//! - `TestPlan.tests` and `TestPlan.rationale` are positionally
//!   aligned: `rationale[i]` explains why `tests[i]` was selected.
//!   A `debug_assert!` in every selector impl guards the invariant.
//! - `selector_version` identifies the selector impl (not the repo
//!   state). Bump on heuristic changes so replay can detect plan
//!   drift without re-running the selector.

use serde::{Deserialize, Serialize};

/// Selector-scoped test identifier. Opaque to azoth-core; the chosen
/// `ImpactSelector` impl decides the exact format. Newtype over
/// `String` so it cannot silently collide with other path-shaped
/// values at call sites.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TestId(pub String);

impl TestId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for TestId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for TestId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl std::fmt::Display for TestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The set of files that changed in a turn (or in the staged overlay,
/// depending on the `DiffSource` the caller wires up). Paths are
/// relative to `ExecutionContext::repo_root`, forward-slashed on
/// every platform so JSONL replay stays portable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    #[serde(default)]
    pub changed_files: Vec<String>,
}

impl Diff {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn from_paths<I, S>(paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            changed_files: paths.into_iter().map(Into::into).collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.changed_files.is_empty()
    }
}

/// An ordered plan produced by an `ImpactSelector`. The runtime
/// persists plans via `SessionEvent::ImpactComputed` (authoritative)
/// plus the `test_impact` SQLite mirror (forensic index).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestPlan {
    pub tests: Vec<TestId>,
    /// `rationale[i]` explains why `tests[i]` was selected. Kept as a
    /// parallel `Vec` rather than a `Vec<(TestId, String)>` so that
    /// per-test confidence / status can be added later without
    /// reshaping the wire format.
    #[serde(default)]
    pub rationale: Vec<String>,
    /// Per-test confidence in `[0.0, 1.0]`. `1.0` = direct heuristic
    /// match. Absent (empty `Vec`) on forward-compat reads — selector
    /// impls are free to leave this unset.
    #[serde(default)]
    pub confidence: Vec<f32>,
    /// Opaque selector-impl version. Bump on heuristic changes.
    pub selector_version: u32,
}

impl TestPlan {
    pub fn empty(selector_version: u32) -> Self {
        Self {
            tests: Vec::new(),
            rationale: Vec::new(),
            confidence: Vec::new(),
            selector_version,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tests.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tests.len()
    }

    /// Internal invariant guard. Callers that build a `TestPlan` by
    /// hand should `debug_assert!(plan.is_well_formed())` before
    /// returning it from a selector.
    pub fn is_well_formed(&self) -> bool {
        self.rationale.len() == self.tests.len()
            && (self.confidence.is_empty() || self.confidence.len() == self.tests.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_from_paths_round_trips() {
        let d = Diff::from_paths(["src/a.rs", "src/b.rs"]);
        assert_eq!(d.changed_files, vec!["src/a.rs", "src/b.rs"]);
        let s = serde_json::to_string(&d).unwrap();
        let back: Diff = serde_json::from_str(&s).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn empty_diff_serialises_with_empty_vec() {
        let d = Diff::empty();
        assert!(d.is_empty());
        let s = serde_json::to_string(&d).unwrap();
        // Forward-compat: `changed_files` has `#[serde(default)]`, so a
        // missing key also parses clean.
        let back: Diff = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(back, d);
        let _roundtrip: Diff = serde_json::from_str(&s).unwrap();
    }

    #[test]
    fn test_plan_empty_is_well_formed() {
        let p = TestPlan::empty(1);
        assert!(p.is_well_formed());
        assert!(p.is_empty());
    }

    #[test]
    fn test_plan_round_trips_with_rationale_alignment() {
        let p = TestPlan {
            tests: vec![TestId::new("crate::foo::tests::bar")],
            rationale: vec!["direct filename match".into()],
            confidence: vec![1.0],
            selector_version: 1,
        };
        assert!(p.is_well_formed());
        let s = serde_json::to_string(&p).unwrap();
        let back: TestPlan = serde_json::from_str(&s).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn mismatched_rationale_length_is_caught_by_invariant() {
        let p = TestPlan {
            tests: vec![TestId::new("a"), TestId::new("b")],
            rationale: vec!["only one rationale".into()],
            confidence: Vec::new(),
            selector_version: 1,
        };
        assert!(!p.is_well_formed());
    }

    #[test]
    fn test_id_string_conversions_work() {
        let a: TestId = "literal".into();
        let b: TestId = "literal".to_string().into();
        assert_eq!(a, b);
        assert_eq!(a.as_str(), "literal");
        assert_eq!(format!("{a}"), "literal");
    }
}
