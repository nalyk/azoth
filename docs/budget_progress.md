# Budget Plan — Progress Tracker

**Current sequence:** β (α merged; awaiting manual kickoff via `/new`)
**Last updated:** 2026-04-24 after α merge
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

**Status:** not-started (α merged 2026-04-24 as `d83eecc`; awaiting user `/new` per sprint-gate-handoff rule)
**Branch:** _(open when kicking off: `feat/budget-beta-amend`)_
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
