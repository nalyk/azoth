# Azoth — Budget Classifier + Amend Plan

## Context

**Why this plan exists.** v2.1.0 shipped 2026-04-24 (commit `c7ac88e`, tag `v2.1.0`, merged PR #29). ~30 minutes after tag push, first real-world dogfood exposed a structural gap: median coding tasks exhaust the `apply_local=20` budget during triage because every `bash` invocation — including pure-read `grep`/`find`/`ls` — is classified `EffectClass::ApplyLocal`. Budget is run-scoped with no amend path (contract lifecycle is `draft → lint → accept`; amend explicitly deferred). Result: user hits budget wall, retry hits the same wall, the only recovery is drafting a new contract (dropping goal continuity).

**Formal cause of the gap** (Aristotle): the "apply_local" budget was designed to serve two distinct concerns — cognitive-cost governor (approvals per unit time) AND mechanical-safety governor (blast-radius cap). These have different natural granularities. Collapsing them left `bash grep` eating the same token as `bash rm`. The sandbox (Landlock tier_a/tier_b) already enforces the mechanical-safety concern. The budget was friction wearing a safety badge.

**Supersession note.** The pre-existing `docs/superpowers/specs/2026-04-21-v2-trilogy-design.md` claims v2.2 for BGE reranker + TOML origin-rule enforcement. This plan is **orthogonal** and does not displace that trilogy. Target version is at user discretion (likely `v2.1.1` patch or its own minor bump).

## Invariants at stake

1. **Invariant #2** — deterministic controls outrank model output. The budget IS part of the deterministic-controls system. The amend path must remain approval-gated; auto-amend is forbidden.
2. **Invariant #4** — every side effect has a class. The classifier refinement preserves the enum exhaustively; it only changes WHICH class bash computes per invocation. `EffectRecord.class` still stores an `EffectClass` (never `None`).
3. **Invariant #7** — turn-scoped atomicity. Amend events land WITHIN a turn, before the terminal marker. An amend does NOT split a turn; the terminal marker remains unique.
4. **Research §10.4** — "scoped-once with session-scope upgradable" was the designed approval shape. The upgrade path was deferred, not cancelled. Amend is completing designed scope, not new scope.

## Key design decisions (proposed — user may override before kickoff)

### α — bash classifier allowlist

Downgrade read-only bash commands from `ApplyLocal` to `Observe`. Bare invocation only; ANY shell metacharacter forces fallback to static `ApplyLocal`.

**Allowlist:**
- Read-only core: `grep rg find ls cat head tail wc file du df stat which sha256sum md5sum xxd od env date pwd test`
- Git read-only: `git log`, `git show`, `git diff`, `git status`, `git blame`, `git rev-parse`, `git branch`, `git tag`, `git ls-files`, `git ls-tree`, `git config --get`
- Cargo read-only: `cargo check`, `cargo metadata`, `cargo tree`, `cargo version`, `cargo --version`
- Other: `rustc --version`, `true`, `false`, `sleep`

**Forbidden shell metacharacters** (force fallback to `ApplyLocal`): `;` `|` `&` `>` `<` `` ` `` `$(` `&&` `||` newline tab `\`

**User override point:** edit `crates/azoth-core/src/tools/bash/classifier.rs::READ_ONLY_COMMANDS` before merging sprint-α.

### β — amend brake parameters

- **≤2 amends per turn** — counter on `EffectCounter`.
- **≤6 amends per run** — counter on `EffectCounter`.
- **≤2× multiplier per amend** — each amend caps at `current_ceiling × 2`; user cannot grant higher.

These are hardcoded in the authority engine, NOT user-configurable within a run. Rationale: prevent social-engineering attacks where the model "talks" the user into raising limits indefinitely.

### γ — default re-tune method

Re-measure with α+δ active before setting new defaults. Expected landing zone: `max_apply_local ∈ [40, 60]`, `max_apply_repo` likely stays at 5. Method is `p95(apply_local_per_task) × 1.25` over the v2.1-J 50-task eval seed.

## Sprint α — Classifier + prompt discipline

### Files to modify

| File | Change |
|---|---|
| `crates/azoth-core/src/execution/dispatcher.rs:35` | Add `fn effect_class_for(&self, _raw: &serde_json::Value) -> Option<EffectClass> { None }` to `Tool` trait (default method — zero-impact for existing impls) |
| `crates/azoth-core/src/execution/dispatcher.rs:57` | Add `fn effect_class_for(&self, raw: &serde_json::Value) -> Option<EffectClass>;` to `ErasedTool` trait (NO default — every impl must route) |
| `crates/azoth-core/src/execution/dispatcher.rs:68` | In `impl<T: Tool + 'static> ErasedTool for T`, route: `fn effect_class_for(&self, raw) -> Option<EffectClass> { <T as Tool>::effect_class_for(self, raw) }` |
| `crates/azoth-core/src/tools/bash.rs:98` | Keep static `effect_class() → ApplyLocal`. Override `effect_class_for(&self, raw)` to parse `raw["command"]` through `classify_bash_command`. |
| `crates/azoth-core/src/tools/bash/classifier.rs` | **NEW** — `classify_bash_command(cmd: &str) -> EffectClass`. Reject any metacharacter; split on whitespace; match first token + optional subcommand against allowlist. |
| `crates/azoth-core/src/turn/mod.rs:798-802` | Change `.map(\|t\| t.effect_class())` → `.map(\|t\| t.effect_class_for(input).unwrap_or_else(\|\| t.effect_class()))` |
| `crates/azoth-core/src/context/constitution.rs` (or wherever tool schemas render) | System-prompt additions (δ): behavior rules, not tool-named. |

### New tests

| Test | Location | Asserts |
|---|---|---|
| `bash_classifier_adversarial` | `crates/azoth-core/tests/bash_classifier_adversarial.rs` | 30+ payloads: every shell metacharacter forces `ApplyLocal`; allowlist positives return `Observe`; `grep; rm -rf /` returns `ApplyLocal` (metachar guard); `grep foo; :` returns `ApplyLocal`; `git log --oneline` returns `Observe`; `git push` returns `ApplyLocal` (not in allowlist); empty string returns `ApplyLocal` |
| `bash_classifier_unit` | inline `#[cfg(test)] mod tests` in `tools/bash/classifier.rs` | Per-command allowlist membership; whitespace handling; leading/trailing spaces |
| `turn_uses_dynamic_classification` | `crates/azoth-core/tests/turn_uses_dynamic_classification.rs` | MockAdapter script: emit `tool_use bash { "command": "grep foo" }` ×25. Assert NO budget-exhaustion abort (because all 25 are `Observe`, not `ApplyLocal`). |

### System-prompt additions (δ, shipped in same PR as α)

In the constitution lane (tool schema pre-amble), add behavior rules:

```
Tool-use discipline:
- Prefer one broad search over many narrow searches. `rg 'pattern' crates/` beats 8 per-crate greps.
- Prefer structured tools (repo_search, repo_read_file, repo_read_spans) over bash when both answer the same question.
- State a 3-5 bullet plan before walking the tree.
- Batch independent reads in a single response when results don't depend on each other.
```

### Ship gate (falsifiable)

- [ ] `cargo test -p azoth-core --test bash_classifier_adversarial -- --nocapture` — green, ≥30 test cases
- [ ] `cargo test --workspace` — green, no regressions
- [ ] `cargo clippy --workspace -- -D warnings` — clean
- [ ] `cargo fmt --check` — clean
- [ ] Manual validation: run `AZOTH_PROFILE=<real> cargo run` against a real codebase, trigger 8 bash-greps in one turn, confirm budget shows single-digit `apply_local` consumed (was previously 8)
- [ ] Both `@gemini` and `@codex` reviewed the final commit and have zero unresolved threads
- [ ] PR merged to main

### Non-goals for α

- Amend path (β). If budget still exhausts during α dogfood, the user can raise the literal in `contract::draft()` temporarily; amend ships in β.
- Cross-tool effect_class_for overrides. Only `BashTool` overrides in α. Adding more tools is v2.2.x or later.
- Eval seed re-run. γ depends on this; don't re-baseline during α.

## Sprint β — Contract amend via approval

### Files to modify

| File | Change |
|---|---|
| `crates/azoth-core/src/schemas/event.rs` | Add `SessionEvent::ContractAmended { contract_id: ContractId, turn_id: TurnId, delta: EffectBudgetDelta, timestamp: String }`. Additive variant — old readers should SKIP unknown event types (verify `JsonlReader` tolerates this) |
| `crates/azoth-core/src/schemas/contract.rs` | Add `EffectBudgetDelta { apply_local: u32, apply_repo: u32, network_reads: u32 }` struct (all additive, unsigned) |
| `crates/azoth-core/src/schemas/effect.rs` | Add `EffectCounter.amends_this_turn: u32`, `amends_this_run: u32`, `turn_id_at_last_amend: Option<TurnId>` (reset `amends_this_turn` on new turn) |
| `crates/azoth-core/src/authority/engine.rs` | New variant `AuthorityDecision::RequireBudgetExtension { current: u32, proposed: u32, label: &'static str, approval_id: ApprovalId }`. Authority engine detects budget-about-to-overflow BEFORE the per-tool approval path |
| `crates/azoth-core/src/turn/mod.rs:812-844` | Rework the budget-overflow branch: instead of aborting, emit `RequireBudgetExtension` approval; on grant → append `ContractAmended` event → update in-memory contract's budget → proceed through normal authorization; on deny → existing abort path |
| `crates/azoth-core/src/event_store/jsonl.rs` | `JsonlReader::committed_run_progress` — fold `ContractAmended` events into the active contract's effective budget (additive accumulation) |
| `crates/azoth-core/src/contract/mod.rs` | Add `pub fn apply_amends(contract: &mut Contract, amends: &[EffectBudgetDelta])` helper (used on resume) |
| `crates/azoth/src/tui/sheet.rs` | Approval modal: render `RequireBudgetExtension` distinctly from per-tool approval (different copy, different glyph, shows `current → proposed` delta) |
| `crates/azoth/src/tui/app.rs` | Handle new approval variant in the approval-bridge worker |

### New tests

| Test | Asserts |
|---|---|
| `contract_amend_round_trips` | Append `ContractAmended` event, re-read via `JsonlReader`, assert effective budget = original + delta |
| `contract_amend_replay` | Seed session with `ContractAccepted(budget=20)` + 10 committed `apply_local` effect_records + `ContractAmended(+20)`. `committed_run_progress` returns `effects_consumed.apply_local = 10, contract.effect_budget.max_apply_local = 40` |
| `contract_amend_rate_limit_per_turn` | 2 amend-grants within one turn OK; 3rd grant returns `AuthorityDecision::NotAvailable { hint: "amend rate limit exceeded: max 2 per turn" }` |
| `contract_amend_rate_limit_per_run` | 6 amend-grants across multiple turns OK; 7th returns NotAvailable |
| `contract_amend_multiplier_cap` | Grant attempts proposed=3× current → clamped to 2×, recorded delta = `current`, NOT `2×current` |
| `contract_amend_turn_atomicity` | Amend event within a turn; turn still has exactly ONE terminal marker (TurnCommitted / TurnAborted / TurnInterrupted) |

### TUI smoke

Manual test procedure (document in progress tracker):
1. Start azoth with `max_apply_local=2` (test-only override)
2. Issue a request that needs 3+ `fs_write`s
3. After 2 writes, approval modal appears for budget extension
4. Grant → third write succeeds
5. Verify `.azoth/sessions/<run_id>.jsonl` contains exactly one `ContractAmended` event
6. Restart session, /resume — confirm budget state reflects the amend

### Ship gate (falsifiable)

- [ ] `cargo test --workspace contract_amend` — 6 new tests green
- [ ] `cargo test -p azoth-core --test turn_enforces_effect_budget` — existing test still green (deny path preserved)
- [ ] `cargo test --workspace` — no regressions
- [ ] Manual TUI smoke passes (documented in progress tracker with screenshot or session excerpt)
- [ ] `cargo clippy --workspace -- -D warnings` — clean
- [ ] Both bots zero unresolved threads; PR merged

### Non-goals for β

- Per-turn amend UX polish (animated rendering of budget delta, historical amend list in inspector). Land the mechanism; polish in v2.2.x.
- Amend for non-budget scope items (e.g., adding paths to `include_paths`). Scope-amend is v2.5.
- Model-initiated amend suggestions. Human-initiated only in β.

## Sprint γ — Default re-tune

### Prerequisites

- Sprint α merged AND in use on `main` for ≥3 real sessions (user's judgement)
- 50-task eval seed re-runnable via `azoth eval run --seed docs/eval/v2_seed_tasks.json`

### Measurement procedure

1. Checkout `main` with α+δ active.
2. Run `azoth eval run --seed docs/eval/v2_seed_tasks.json --live-retrieval <real-repo>` ×3 iterations.
3. For each run, collect per-task `apply_local` count from JSONL `effect_record` aggregations.
4. Compute `p95` across all runs pooled.
5. New default = `ceil(p95 × 1.25 / 5) * 5` (round to nearest 5 for tidiness).

### Files to modify

| File | Change |
|---|---|
| `crates/azoth-core/src/contract/mod.rs:35` | `max_apply_local: <computed>` (expected 40-60) |
| `crates/azoth-core/src/contract/mod.rs:36` | `max_apply_repo: 5` (likely unchanged; re-measure to confirm) |
| `CHANGELOG.md` | Explicit entry: `Default max_apply_local raised from 20 to N following classifier refinement in sprint-α. Rationale: with bash-of-grep now classified Observe, the remaining ApplyLocal budget reflects actual write pressure; new ceiling is p95 × 1.25 over 50-task eval seed.` |
| `README.md` | If it documents defaults anywhere (grep first), sync the number. |
| `docs/draft_plan.md` or `docs/v2_plan.md` | If either references `max_apply_local=20`, sync. |

### Ship gate (falsifiable)

- [ ] Measurement artifact committed to `docs/eval/budget_measurement_<date>.md` (raw numbers, not prose)
- [ ] `cargo test --workspace` — green with new defaults
- [ ] CHANGELOG entry reviewed for no aspirational claims (apply `pattern_grep_verify_release_notes_against_code.md`)
- [ ] Both bots zero unresolved threads; PR merged

### Non-goals for γ

- Per-profile defaults (anthropic vs openai vs ollama). Static default in v1.
- User-configurable defaults from the CLI. Edit `contract::draft()` or use a config file (already supported); the default is just the DEFAULT.

## Cross-sprint verification gates (for future eval-plane hardening)

These should be ADDED to the eval harness during γ so v2.2+ sprints catch this class of regression earlier:

- [ ] **Gate G1 — Budget survival rate:** `> 95%` of eval seed tasks complete without budget abort under default contract. Ships with γ.
- [ ] **Gate G2 — Tool-call efficiency:** median turn's bash-grep count `≤ 2` post-δ. Shipped with α telemetry hook.
- [ ] **Gate G3 — Amend correctness:** eval harness includes ≥2 tasks that require budget extension; asserts amend grant resumes the same tool call. Ships with β.

## Risk ledger

1. **Classifier false-downgrade under adversarial payload.** Allowlist + metachar-reject prevents. Sandbox (Landlock) remains the real defense. Adversarial test suite in α makes this falsifiable.
2. **Amend jailbreak — model talks user into raising budget indefinitely.** Three structural brakes (≤2/turn, ≤6/run, ≤2× multiplier) prevent unbounded growth. Each amend is logged and visible in the inspector.
3. **JSONL forward-compat on old binaries reading new events.** New `SessionEvent::ContractAmended` variant must be either (a) `#[serde(other)]` skipped by old readers, or (b) treated as an error. Verify `JsonlReader` behavior on unknown variants; add a test.
4. **TUI layout regression on new approval modal variant.** Visual regression risk. Manual smoke required (no auto-test for TUI layout).
5. **Measurement flakiness in γ.** `p95` across 3 runs × 50 tasks may still have noise. Report p95 ± variance; if variance > 20%, add 2 more runs before setting the default.

## Out of scope

- **LSP integration** — already deferred to v2.5 per `pattern_defer_reasons_outlive_calendar_slot`.
- **BGE cross-encoder reranker** — already in the v2.2 trilogy plan; independent from this work.
- **TOML origin-rule enforcement** — already in the v2.2 trilogy plan.
- **Tier C/D sandboxes** — v2.5.
- **Non-bash classifier overrides** — wait for real-world signal.

## Versioning note

Semver mapping of the three sprints:
- α = patch (behavior change, no API change) → candidate for `v2.1.1`
- β = minor (new public API: `SessionEvent::ContractAmended`, `AuthorityDecision::RequireBudgetExtension`) → candidate for `v2.2.0` or `v2.1.2` (backward-compatible additions — minor per strict semver; patch acceptable if treated as bug fix)
- γ = patch (default value change)

User decides the exact version bump at each merge. Plan is version-agnostic.
