# Budget Plan — Progress Tracker

**Current sequence:** β (in-flight on branch `feat/budget-beta-amend`)
**Last updated:** 2026-04-24 after β.R0 build
**Plan reference:** [`docs/budget_plan.md`](./budget_plan.md)

---

## Sprint α — Classifier + prompt discipline

**Status:** MERGED
**Branch:** `feat/budget-alpha-classifier` (deleted on merge)
**PR:** [#30](https://github.com/nalyk/azoth/pull/30)
**Merged at:** `d83eecc` on 2026-04-24 (squash of R0–R4)

### Subtasks

- [x] Open feature branch `feat/budget-alpha-classifier` from `main`
- [x] Add `fn effect_class_for(&self, _raw: &Value) -> Option<EffectClass> { None }` default to `Tool` trait (`crates/azoth-core/src/execution/dispatcher.rs:35`)
- [x] Add `fn effect_class_for(&self, raw: &Value) -> Option<EffectClass>;` to `ErasedTool` trait (no default) at `dispatcher.rs:57`
- [x] Route in `impl<T: Tool + 'static> ErasedTool for T` at `dispatcher.rs:68`
- [x] Create `crates/azoth-core/src/tools/bash/` module dir + `classifier.rs`
- [x] Implement `classify_bash_command(cmd: &str) -> EffectClass` per allowlist in plan §α
- [x] Override `BashTool::effect_class_for` in `tools/bash.rs`
- [x] Wire into budget-check at `crates/azoth-core/src/turn/mod.rs:798-802`
- [x] Add `crates/azoth-core/tests/bash_classifier_adversarial.rs` with ≥30 payloads (17 test fns / ~93 payloads)
- [x] Add inline unit tests in `classifier.rs` (13 per-command allowlist membership tests)
- [x] Add `crates/azoth-core/tests/turn_uses_dynamic_classification.rs` (bash `ls` at apply_local cap does not abort)
- [x] Add system-prompt behavior rules (δ) to constitution lane — `crates/azoth-core/src/context/discipline.rs` + injected into `turn/mod.rs` constitution formatter
- [x] Run `cargo fmt --check` clean
- [x] Run `cargo clippy --workspace -- -D warnings` clean
- [x] Run `cargo test --workspace` — 795 passed, 0 failed (serial `--test-threads=1`; parallel mode has the pre-existing WSL2 Tier-B overlay flake, unrelated)
- [x] Adversarial self-review pass per `feedback_adversarial_self_review_before_push.md` — null-safety ladder on `raw.get("command")?.as_str()?`, bytewise metachar check safe for Unicode, quoted metachars conservatively fall through to ApplyLocal, two-layer safety preserved (sandbox from static class / budget+authority from dynamic)
- [x] Update this tracker: subtasks ticked, session log appended
- [x] Commit + push + open PR (opened as #30 with commit 1637c2d)
- [x] Trigger bot review: `/gemini review` and `@codex review` top-level comments
- [x] Wait ≥5 min after trigger (per `feedback_wait_for_bot_processing_after_rereview.md`)
- [x] DUAL-query PR reviewThreads (GraphQL + REST) per `feedback_dual_query_immediately_before_every_push.md`
- [x] Address findings; push rounds; re-trigger; re-query (5 rounds R0–R4)
- [x] Loop until both bots have zero unresolved threads (gemini R3+R4 clean; codex R4 implicit pass; 1 declined-with-docs thread on `--log-file=`)
- [x] Merge PR to main (squashed as `d83eecc` on 2026-04-24)
- [x] Update tracker: mark sequence complete, log merge commit SHA + date (this commit)
- [x] Commit tracker update to main (this commit, direct to main per `feedback_amend_in_place_when_reversing_a_defer` fallback clause)

### Gates (all checked — α complete)

- [x] `cargo test -p azoth-core --test bash_classifier_adversarial -- --nocapture` — 27 integration test fns, ~100+ payloads
- [x] `cargo test --workspace` — 812 passed / 0 failed serial (up from 795 at R0; +17 new regression tests across R1–R4)
- [ ] Manual validation on real codebase: 8× bash-grep in one turn consumes 0 apply_local (not 8) — **DEFERRED to post-merge dogfood** per `Non-goals for α` in plan
- [x] Both bots zero unresolved active findings (15 residue threads all last-by-nalyk with inline replies; none re-raised)
- [x] PR merged to main with tracker update landed

### Session log

- **2026-04-24 (planning)** — Plan drafted in `docs/budget_plan.md`. Awaiting first kickoff session.
- **2026-04-24 R0 build** — implemented classifier + hook + wiring + δ rules in one commit `1637c2d`. 795/0 serial. Files: `execution/dispatcher.rs`, `tools/mod.rs`, `tools/bash.rs`, `tools/bash/classifier.rs` (new), `turn/mod.rs`, `context/mod.rs`, `context/discipline.rs` (new), 2 new integration tests, 13 inline unit tests.
- **2026-04-24 R0 push + open PR** — Pushed main (plan commit `d4f9e81`) and feature branch. Opened PR #30. Triggered `/gemini review` + `@codex review`.
- **2026-04-24 R1 `3826309`** — Addressed 6 R0 findings (gemini HIGH×2 + gemini critical + codex P1×3, all duplicates of two class bugs). Removed `find` + `env` + `git branch` + `git tag` from allowlists; added `has_write_flag` scan for `--output*` tokens across all argv. +9 regression tests. 804/0 serial.
- **2026-04-24 R2 `4281698`** — Addressed 3 R1 findings (gemini R1 HIGH + codex R1 P1×2). Added `'`/`"` to `has_forbidden_metachar` (quote bypass of `has_write_flag` — shell strips quotes but my prefix check preserved them, silent escape). Removed `xxd` (its `-r` reverse mode writes). +4 regression tests. 808/0 serial. New pattern memo written: `pattern_flag_scan_plus_split_whitespace_equals_quote_bypass.md`.
- **2026-04-24 R3 `b9d1c07`** — Addressed 4 R2 findings (gemini MED×2 + codex P1×2); applied 3, declined-with-docs 1. Removed cargo subcommand allowlist entirely (`cargo check --target-dir` writes artifacts anywhere). Removed `date` (`-s STRING` sets clock). Flipped unknown-tool default from Observe to ApplyLocal. Declined `--log-file=` scope expansion — verified `rg` has no such flag; defensive-gate-needs-proof. +4 regression tests. 810/0 serial.
- **2026-04-24 R4 `18a4c83`** — Addressed 1 R3 finding (codex P1 on `git diff` / `git status` invoking `refresh_index()` which writes `.git/index` when stat cache is stale). Removed both from `GIT_READ_ONLY_SUBCOMMANDS`. Gemini R3 review: *"I have no feedback to provide as the changes are well-documented and thoroughly tested."* +2 regression tests. 812/0 serial. New pattern memo written: `pattern_git_diff_status_write_index_via_refresh_index.md`.
- **2026-04-24 R5 PR-body rewrite (no commit)** — Addressed 1 R4 gemini MED on PR description being stale (listed `diff`/`status` as Observe after R4 removed them). Rewrote PR body via `gh api -X PATCH` because `gh pr edit --body` silently failed on Projects-Classic deprecation GraphQL warning. New pattern memo: `pattern_gh_pr_edit_body_silently_fails_on_projects_classic_deprecation.md`. Codex R4 review: implicit pass (no review posted — codex skipped when no net-new findings vs R3 output).
- **2026-04-24 α merge** — PR #30 squashed to main as `d83eecc`. Branch `feat/budget-alpha-classifier` deleted on merge. 15 review threads total across 5 rounds, all resolved to my satisfaction (14 applied + 1 declined-with-docs; no active blockers). User's sprint-gate-handoff rule now active — β+γ blocked on user `/new`.

### Final allowlists after R0→R4

```
READ_ONLY_COMMANDS (20 entries): grep, rg, ls, cat, head, tail, wc, file,
  du, df, stat, which, sha256sum, md5sum, od, pwd, test, true, false, sleep
Removed across rounds: find, env (R1), xxd (R2), date (R3)

GIT_READ_ONLY_SUBCOMMANDS (6): log, show, blame, rev-parse, ls-files, ls-tree
  + git config --get (special case)
Removed across rounds: branch, tag (R1), diff, status (R4)

CARGO_READ_ONLY_SUBCOMMANDS: removed entirely in R3 (cargo --target-dir escape)

RUSTC: --version only → Observe; everything else ApplyLocal
```

Forbidden metachars in `has_forbidden_metachar`: `; | & > < \` $ ( ) \ \n \t \r ' "`

`has_write_flag` scan rejects any token equal to `--output` or starting with `--output=` (tight match to preserve `--output-format`, `--output-indicator-new`).

### Patterns extracted during sprint α

- `pattern_flag_scan_plus_split_whitespace_equals_quote_bypass.md` (R2) — reusable for future classifier extensions.
- `pattern_git_diff_status_write_index_via_refresh_index.md` (R4) — reusable for future git-command classification.
- `pattern_gh_pr_edit_body_silently_fails_on_projects_classic_deprecation.md` (R5) — reusable for any future `gh pr edit --body` on this repo.

---

## Sprint β — Contract amend via approval

**Status:** in-flight (R0 built; awaiting user push consent)
**Branch:** `feat/budget-beta-amend`
**PR:** _(link when pushed)_
**Merged at:** _(commit SHA + date)_

### Subtasks

- [x] Open feature branch `feat/budget-beta-amend` from `main` (after α has merged)
- [x] Add `EffectBudgetDelta` struct in `crates/azoth-core/src/schemas/contract.rs`
- [x] Add `SessionEvent::ContractAmended` variant in `crates/azoth-core/src/schemas/event.rs`
- [x] Verify `JsonlReader` skips unknown variants gracefully; add regression test `jsonl_tolerates_unknown_event_variant` — chose LOUD-failure semantics (documented in plan §β risk #3): unknown variant is a `ProjectionError::Parse`, not a silent skip. Test `unknown_event_variant_is_a_loud_parse_error_not_a_silent_skip` in `contract_amend_round_trips.rs`.
- [x] Extend `EffectCounter` (in `schemas/effect.rs`) with amend counters + per-turn reset hook — six new u32 fields, `Copy` preserved; reset at `drive_turn` entry
- [x] Add `AuthorityDecision::RequireBudgetExtension` variant at `authority/engine.rs` + `authorize_budget_extension` method enforcing the ≤2/turn + ≤6/run brakes
- [x] Rework turn/mod.rs budget-overflow branch at line 812-844 — replace abort with approval request
- [x] Add `apply_amends` helper in `contract/mod.rs` + `apply_amend_clamped` + `apply_amend_clamped_against_base` (2× multiplier cap)
- [x] Fold `ContractAmended` in `JsonlReader::committed_run_progress` + new `last_effective_contract` method
- [x] TUI approval modal — new variant rendering in `crates/azoth/src/tui/sheet.rs` (distinct title + body for `budget_extension`)
- [x] Approval bridge worker handles new variant in `crates/azoth/src/tui/app.rs` (no handler change needed — same Grant/Deny surface; driver ignores scope on amend grant)
- [x] Add `contract_amend_round_trips.rs` test (3 tests)
- [x] Add `contract_amend_replay.rs` test (2 tests)
- [x] Add `contract_amend_rate_limit_per_turn.rs` test (2 tests)
- [x] Add `contract_amend_rate_limit_per_run.rs` test (2 tests)
- [x] Add `contract_amend_multiplier_cap.rs` test (6 tests)
- [x] Add `contract_amend_turn_atomicity.rs` test (1 integration test)
- [ ] Manual TUI smoke test (documented in this tracker) — deferred per plan §β "TUI smoke" (procedure documented; manual run after PR open)
- [x] `cargo fmt --check` clean
- [x] `cargo clippy --workspace -- -D warnings` clean
- [x] `cargo test --workspace` — all green (829 passed / 0 failed serial; up from 812 at end of α — 17 new tests)
- [x] Adversarial self-review pass (static-str alloc on `ext.label` caught + fixed; `turn_enforces_effect_budget` seeded to trip the per-run brake to preserve the deny-path contract; telemetry `budget_extension` discriminator documented)
- [x] Update tracker: check off subtasks, session log (this edit)
- [ ] Commit + push + open PR
- [ ] Trigger bot review, dual-query, address, loop until both bots zero unresolved
- [ ] Merge to main
- [ ] Tracker sync on main

### Gates

- [x] 6 new amend tests green (17 test fns total across six files + one inline round-trip in event.rs)
- [x] `turn_enforces_effect_budget` still green — deny path preserved by seeding `amends_this_run = MAX_AMENDS_PER_RUN` so the brake trips and NotAvailable → RuntimeError abort fires
- [ ] Manual TUI smoke passes (procedure in plan §β)
- [ ] Both bots zero unresolved threads
- [ ] PR merged

### Session log

- **2026-04-24 β.R5** — addressed 3 R4 findings (1 class bug, 2 surfaces caught by gemini MED + codex P1 ×2). My R4 `reset_for_new_contract` zeroed `amends_this_run` — contradicting my own R2 fix that explicitly preserves it across `ContractAccepted` in `fold_progress` to prevent per-run brake bypass via contract cycling. Both bots caught the self-contradiction independently. Removed the zeroing; method now only clears the three `*_ceiling_bonus` fields. Updated unit test to assert `amends_this_run` survives the reset. Second codex P1 (turn/mod.rs:912) pointed out that `/contract <goal>` IS a real mid-session replacement path in the TUI — my R4 claim "β doesn't support mid-session replacement" was FALSE. Hooked `effects_consumed.reset_for_new_contract()` in app.rs at both `accept_and_persist` sites (the explicit `/contract` path at line 2002 and the auto-draft fallback at line 2106). Fixed the doc comment on the method to reflect the actual wire-up and correct the wrong scope claim. 835/0 (unit test count unchanged; behaviour change only).

- **2026-04-24 β.R4** — addressed 2 late-arriving R3 codex findings (caught by the extended 4-min cushion, not the initial 6-min wait — PR #24 R2 scenario confirmed). **codex P1 turn/mod.rs:885**: live driver's `apply_X_ceiling_bonus` never clears on contract change while `fold_progress` resets on `ContractAccepted` — live-vs-replay divergence under mid-session contract replacement. Added `EffectCounter::reset_for_new_contract()` public method that zeroes the three `*_ceiling_bonus` fields + `amends_this_run`; documented the worker-side call-site contract. β's built-in TUI doesn't do mid-session replacement but the method prevents future drift. **codex P2 turn/mod.rs:1221**: after amend grant, code fell through to per-tool auth even when `used` was STILL above the new ceiling (reachable via resume-into-stricter-contract or harness pre-seed). Added post-grant re-check that computes `new_effective_max` and aborts `RuntimeError` with "effect budget still exhausted after amend" detail if `used >= new_effective_max`. `ContractAmended` stays written before the abort (audit trail); replayable drops aborted turn whole. +2 regression tests: unit test on `reset_for_new_contract` locks the field-zeroing contract; integration test `amend_grant_that_does_not_clear_budget_aborts_before_tool_dispatch` pre-seeds `apply_local=50` against `max_apply_local=1`, asserts abort before tool dispatch with the new detail string. 835/0.

- **2026-04-24 β.R3 `e19ecf5`** — addressed 2 R2 findings (same class bug: codex P2 + gemini HIGH caught it independently). When a turn grants TWO amends, the second grant's `apply_amend_clamped_against_base` call was passing bare `bonus` as its ceiling argument, ignoring the `pending_for_class` delta from the first grant. Result: second grant under-applied — user approved a 4-unit ceiling but only 3 units landed. Fixed by passing `bonus.saturating_add(pending_for_class)` to the clamp, restoring symmetry with the budget-check's own use of pending-inclusive ceiling. +1 regression test `second_amend_in_same_turn_clamps_against_pending_inclusive_ceiling` drives a two-fs_write MockScript at max_apply_local=1 with pre-seeded counter at cap; asserts `apply_local_ceiling_bonus == 3` (first amend 1 + second amend 2), not 2 (the pre-R3 buggy sum). 833/0.

- **2026-04-24 β.R2** — addressed 6 R1 findings (4 applied + 2 reply-only). Two codex findings were real correctness bugs: (1) **codex P1** — amend state updates were immediate, so an aborted turn leaked the ceiling bonus + run-brake bump into next turn's live memory while the replayable projection dropped the ContractAmended event (aborted turns drop whole). Fixed by routing grants through four stack-local `pending_amend_*` variables in `drive_turn`; the budget check adds `pending_for_class` to the effective ceiling for in-turn visibility; a commit-time flush in the `TurnCommitted` write site transfers pending into `effects_consumed`; every abort path drops pending naturally via stack scope. (2) **codex P2** — `fold_progress` was resetting `amends_this_run` on every `ContractAccepted`, which would have let a user cycle contracts to bypass `MAX_AMENDS_PER_RUN`. Fixed by preserving the per-run counter across contract boundaries; only the `*_ceiling_bonus` triplet resets. **gemini MED turn/mod.rs:1098** — dropped `ApprovalGranted` emission on the amend grant path; amend doesn't mint a capability, so the token ID was a false forensic signal. `ContractAmended` remains as invariant-#5 evidence. **gemini MED turn/mod.rs:904** — inlined the amend-branch AuthorityEngine construction via method chaining (one-liner instead of a scope block). Two reply-only: **gemini MED jsonl.rs:617** (duplicate of already-applied fix — the code IS single-pass after R1) and **gemini MED jsonl.rs:665** (effect-tally non-reset asymmetry; pre-β scope, documented). +1 regression test: `aborted_turn_after_amend_does_not_persist_amend_state` (grant then deny → counter stays pre-turn). Adjusted `fold_progress_ignores_stale_amends_across_contract_replacement` to expect `amends_this_run == 2` (both amends count against the run brake, regardless of contract replacement).

- **2026-04-24 β.R1** — addressed 5 R0 findings (2 gemini HIGH + 1 gemini MED + 1 codex P1 + 1 codex P2). Core class bug: `fold_progress` accumulated `ContractAmended` deltas across contract-id boundaries, creating an asymmetry with `last_effective_contract` (which already scoped by contract_id). Fixed by tracking `current_contract_id` in `fold_progress` and resetting `apply_X_ceiling_bonus` + `amends_this_run` on each `ContractAccepted`. `last_effective_contract` rewritten as a single-pass (one `scan()`, one in-memory slice iteration) addressing gemini HIGH #1 inefficiency. `authorize_budget_extension` now rejects `current == 0` as `NotAvailable { hint: "amend cannot extend a zero ceiling" }` — prevents the codex P1 "zero-delta grant is a budget bypass" scenario. `BudgetExtensionRequest` doc updated to explain that `network_reads` is scaffolding matching `EffectBudget`'s three-field shape + that `current == 0` is structurally impossible at the TUI surface. +2 regression tests: `fold_progress_ignores_stale_amends_across_contract_replacement` + `zero_current_is_not_available_even_when_brakes_clear`. Full workspace 831/0 serial.

- **2026-04-24 β.R0 build** — implemented β end-to-end in one commit series on `feat/budget-beta-amend`:
  - Schemas: `EffectBudgetDelta`, `SessionEvent::ContractAmended`, `EffectCounter` extension (6 new u32 fields, `Copy` preserved).
  - Authority: `AuthorityDecision::RequireBudgetExtension`, `ApprovalRequestMsg.budget_extension: Option<BudgetExtensionRequest>`, `authorize_budget_extension` enforcing the two brake constants `MAX_AMENDS_PER_TURN=2` + `MAX_AMENDS_PER_RUN=6` + `AMEND_PROPOSED_MULTIPLIER=2`.
  - Turn driver: replaced the `used >= max` abort with an amend-offer branch; on grant writes `ApprovalGranted` + `ContractAmended`, bumps `apply_X_ceiling_bonus`, increments `amends_this_turn` + `amends_this_run`, falls through to the normal per-tool authorization (amend raises the ceiling, does NOT pre-authorize the tool). On deny, aborts with `ApprovalDenied`. On brake tripped (NotAvailable), aborts with `RuntimeError` carrying the exact hint string.
  - Reset semantics: `amends_this_turn = 0` at `drive_turn` entry so the per-turn brake is actually per-turn; `amends_this_run` never reset; `turn_id_at_last_amend` dropped from plan (simpler: the reset is explicit on drive_turn entry).
  - Contract helpers: `apply_amend_clamped`, `apply_amend_clamped_against_base`, `apply_amends` (replay fold).
  - JSONL replay: `fold_progress` folds `ContractAmended` deltas into `apply_X_ceiling_bonus`; new `last_effective_contract()` returns the accepted contract with amends folded in. Amends matched by `contract_id` so a mid-session `ContractAccepted` supersedes prior amends.
  - TUI: `sheet.rs` renders a distinct title + body when `budget_extension: Some(..)` — "extend apply_local: 20 → 40" header + "granting raises the ceiling only" explainer. `app.rs` handler untouched (existing Grant/Deny surface maps 1:1 — driver ignores scope on amend).
  - Tests: 16 new test fns across 6 files (`contract_amend_round_trips`, `contract_amend_replay`, `contract_amend_rate_limit_per_turn`, `contract_amend_rate_limit_per_run`, `contract_amend_multiplier_cap`, `contract_amend_turn_atomicity`). Per-file results: 3+2+2+2+6+1 = 16, all green.
  - Three existing tests updated to `..Default::default()` for the `EffectCounter` field-literal construction: `turn_enforces_effect_budget.rs`, `turn_uses_dynamic_classification.rs`, `resume_recomputes_effects_and_turns.rs`. Additive only — existing semantics preserved.
  - One planning divergence: brake parameters shipped with plan values (2/turn, 6/run, 2× multiplier). No consultation needed — they were unambiguous in plan §β "Key design decisions".

---

## Sprint γ — Default re-tune

**Status:** not-started (blocked on α merge + 50-task eval re-run)
**Branch:** _(open when ready: `feat/budget-gamma-defaults`)_
**PR:** _(link when pushed)_
**Merged at:** _(commit SHA + date)_

### Prerequisites

- α merged to main
- ≥3 real sessions on main (user judgement)
- Eval harness capable of per-task `apply_local` count aggregation

### Subtasks

- [ ] Run `azoth eval run --seed docs/eval/v2_seed_tasks.json --live-retrieval <repo>` ×3
- [ ] Collect per-task `apply_local` from JSONL
- [ ] Compute `p95` across pooled runs
- [ ] Compute new default = `ceil(p95 × 1.25 / 5) × 5`
- [ ] Report p95 ± variance in `docs/eval/budget_measurement_<date>.md`
- [ ] If variance > 20%, add 2 more iterations before committing
- [ ] Update `crates/azoth-core/src/contract/mod.rs:35` with new literal
- [ ] Update CHANGELOG.md with explicit number + rationale (no aspirational claims)
- [ ] Grep for `max_apply_local` documentation mentions in `docs/`, `README.md`; sync
- [ ] `cargo test --workspace` — green with new defaults
- [ ] Adversarial self-review pass
- [ ] Commit + push + open PR
- [ ] Trigger bot review, dual-query, address, loop until both bots zero unresolved
- [ ] Merge to main
- [ ] Tracker sync on main

### Gates

- [ ] Measurement artifact committed to `docs/eval/`
- [ ] Variance ≤ 20% (or documented with extra iterations)
- [ ] CHANGELOG entry has no aspirational claims (apply `pattern_grep_verify_release_notes_against_code.md`)
- [ ] Both bots zero unresolved threads
- [ ] PR merged

### Session log

_(empty — pending α merge)_

---

## Cross-sprint verification gates (ship with γ)

- [ ] **G1 — Budget survival rate** (≥95% of eval seed tasks complete without budget abort)
- [ ] **G2 — Tool-call efficiency** (median bash-grep per turn ≤ 2 post-δ)
- [ ] **G3 — Amend correctness** (≥2 eval tasks require + grant + resume amend path)

---

## Open questions / blockers

_None currently. User to confirm brake parameters (≤2/turn, ≤6/run, ≤2× multiplier) and the bash-classifier allowlist before kickoff; amend plan inline to override if different._

---

## Tracker update rule

**Every session that touches code MUST update this file.** Specifically:
1. On subtask completion — check the box.
2. On each push — add a session log entry with date + brief round description.
3. On each review round — note bot findings and disposition (addressed / rejected-with-docs / deferred).
4. On PR merge — log merge commit SHA + date, flip sequence status to `merged`, advance `Current sequence:` pointer at the top of this file.

Commit the tracker update as part of the feature PR (so the merge brings it to main in one shot). If a tracker-only update is needed on main outside a feature PR, use the commit message format `azoth: budget-tracker sync — <what changed>`.
