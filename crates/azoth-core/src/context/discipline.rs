//! Tool-use discipline rules shipped in the constitution lane.
//!
//! These are behavior rules, not tool-named rules — they tell the
//! model HOW to use the tool menu, not WHICH tools exist. The
//! constitution lane is cache-prefix-stable; editing this constant
//! invalidates prompt caches across providers on the next deploy
//! (expected and acceptable for a policy-layer change).
//!
//! The rules close the feedback loop on the v2.1.0 post-merge
//! dogfood gap documented in
//! `project_azoth_status_apr24_v86_v2_1_0_dogfood_budget_gap.md`:
//! the median turn was emitting 8+ narrow `bash grep` calls where
//! one broad `rg` call would have answered the same question.
//! Sprint α downgrades read-only bash to `Observe` on the policy
//! side; this δ addition tells the model not to burn the budget
//! anyway out of habit.

/// Tool-use discipline preamble. Rendered into the constitution
/// lane by `turn::drive_turn` after the contract digest header and
/// before the caller-provided `system_prompt`. Keep this short —
/// every byte is paid for at every turn.
pub const TOOL_USE_DISCIPLINE: &str = "\
[azoth.tool_discipline]
- Prefer one broad search over many narrow searches. `rg 'pattern' crates/` beats eight per-crate greps.
- Prefer structured tools (repo_search, repo_read_file, repo_read_spans) over bash when both answer the same question.
- State a 3-5 bullet plan before walking the tree.
- Batch independent reads in a single response when results don't depend on each other.";
