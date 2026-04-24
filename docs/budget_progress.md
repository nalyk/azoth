# Budget Plan — Progress Tracker

**Current sequence:** α (in-review; PR opened, awaiting bot review rounds)
**Last updated:** 2026-04-24 by α implementation session
**Plan reference:** [`docs/budget_plan.md`](./budget_plan.md)

---

## Sprint α — Classifier + prompt discipline

**Status:** in-review (PR open, R0 pushed, awaiting bot review rounds)
**Branch:** `feat/budget-alpha-classifier`
**PR:** _(link populated on first push)_
**Merged at:** _(commit SHA + date)_

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
- [ ] Commit + push + open PR
- [ ] Trigger bot review: `/gemini review` and `@codex review` top-level comments
- [ ] Wait ≥5 min after trigger (per `feedback_wait_for_bot_processing_after_rereview.md`)
- [ ] DUAL-query PR reviewThreads (GraphQL + REST) per `feedback_dual_query_immediately_before_every_push.md`
- [ ] Address findings; push rounds; re-trigger; re-query
- [ ] Loop until both bots have zero unresolved threads
- [ ] Merge PR to main
- [ ] Update tracker: mark sequence complete, log merge commit SHA + date
- [ ] Commit tracker update to main if not already merged

### Gates (must all check before declaring α complete)

- [ ] `cargo test -p azoth-core --test bash_classifier_adversarial -- --nocapture` passes with ≥30 cases
- [ ] `cargo test --workspace` — no regressions
- [ ] Manual validation on real codebase: 8× bash-grep in one turn consumes 0 apply_local (not 8)
- [ ] Both bots zero unresolved threads
- [ ] PR merged to main with tracker update landed

### Session log

- **2026-04-24 (planning)** — Plan drafted in `docs/budget_plan.md`. Awaiting first kickoff session.
- **2026-04-24 (α R0 build)** — implemented classifier + hook + wiring + δ rules in one commit. Baseline tests clean minus two pre-existing WSL2 Tier-B flakes under `--test-threads>1` (documented in memory). Post-implementation: 795 passed / 0 failed serial, `cargo clippy -D warnings` clean, `cargo fmt --check` clean. Files touched: `execution/dispatcher.rs` (trait hooks), `tools/mod.rs` (bash pub), `tools/bash.rs` (hook override), `tools/bash/classifier.rs` (new), `turn/mod.rs` (dynamic class + discipline injection), `context/mod.rs` (discipline export), `context/discipline.rs` (new), 2 new integration tests, 13 inline unit tests. Awaiting user consent to push main + feature branch.

---

## Sprint β — Contract amend via approval

**Status:** not-started (blocked on α merge)
**Branch:** _(open when α lands: `feat/budget-beta-amend`)_
**PR:** _(link when pushed)_
**Merged at:** _(commit SHA + date)_

### Subtasks

- [ ] Open feature branch `feat/budget-beta-amend` from `main` (after α has merged)
- [ ] Add `EffectBudgetDelta` struct in `crates/azoth-core/src/schemas/contract.rs`
- [ ] Add `SessionEvent::ContractAmended` variant in `crates/azoth-core/src/schemas/event.rs`
- [ ] Verify `JsonlReader` skips unknown variants gracefully; add regression test `jsonl_tolerates_unknown_event_variant`
- [ ] Extend `EffectCounter` (in `schemas/effect.rs`) with amend counters + per-turn reset hook
- [ ] Add `AuthorityDecision::RequireBudgetExtension` variant at `authority/engine.rs`
- [ ] Rework turn/mod.rs budget-overflow branch at line 812-844 — replace abort with approval request
- [ ] Add `apply_amends` helper in `contract/mod.rs`
- [ ] Fold `ContractAmended` in `JsonlReader::committed_run_progress`
- [ ] TUI approval modal — new variant rendering in `crates/azoth/src/tui/sheet.rs`
- [ ] Approval bridge worker handles new variant in `crates/azoth/src/tui/app.rs`
- [ ] Add `contract_amend_round_trips.rs` test
- [ ] Add `contract_amend_replay.rs` test
- [ ] Add `contract_amend_rate_limit_per_turn.rs` test
- [ ] Add `contract_amend_rate_limit_per_run.rs` test
- [ ] Add `contract_amend_multiplier_cap.rs` test
- [ ] Add `contract_amend_turn_atomicity.rs` test
- [ ] Manual TUI smoke test (documented in this tracker)
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo test --workspace` — all green
- [ ] Adversarial self-review pass
- [ ] Update tracker: check off subtasks, session log
- [ ] Commit + push + open PR
- [ ] Trigger bot review, dual-query, address, loop until both bots zero unresolved
- [ ] Merge to main
- [ ] Tracker sync on main

### Gates

- [ ] 6 new amend tests green
- [ ] `turn_enforces_effect_budget` still green (deny path preserved)
- [ ] Manual TUI smoke passes (procedure in plan §β)
- [ ] Both bots zero unresolved threads
- [ ] PR merged

### Session log

_(empty — pending α merge)_

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
