# Post-E2E gap fixes (F1–F9) Implementation Plan

> **For agentic workers:** This plan executes inline in the author session, TDD per fix, one PR against `main`.

**Goal:** Fix the 9 findings from the 2026-04-24 TUI conversational E2E (`.azoth/test-reports/intel.md`) so the next dogfood run produces zero repeat reports.

**Architecture:** Each finding gets a dedicated commit with failing test → minimal fix → green test. All nine land on one branch `feat/post-e2e-gap-fixes-f1-to-f9`; one PR #33. The changes span `whisper.rs`, `app.rs`, `schemas/effect.rs`, `sheet.rs`, `adapter/profile.rs`, `tui/config.rs`, and `CLAUDE.md`.

**Tech stack:** Rust stable, ratatui 0.30, crossterm 0.29, serde/serde_json, tokio 1.x; all existing deps. No new crates.

**Validation I already performed:**
- F1 confirmed — `whisper::render_line` at `tui/whisper.rs:53` takes `(theme, latest_note)` with no awareness of `pending_approval`.
- F2 confirmed — `PaletteAction::Continue` at `tui/app.rs:376-387` unconditionally queues continuation text; never inspects last terminator.
- F3 confirmed — `max_context_tokens: 32_768` hardcoded at `tui/config.rs:42,52` AND `adapter/profile.rs:98,117` (ollama profiles). Azoth-side budget rejects BEFORE model sees the packet.
- F4 confirmed — `TurnStarted { turn_id, .. }` at `app.rs:1050` destructure discards `timestamp`; `TurnCommitted { turn_id, usage, .. }` at `app.rs:1337` discards `at`. Card is populated with `SystemTime::now()` unconditionally. Schema at `schemas/event.rs:172,183,192` DOES carry `at: Option<String>`; Chronon CP-1 writer at `turn/mod.rs:1876` always populates it.
- F5 partial — banner exists as a 5s note at `app.rs:2341-2348` but only contains `resumed · <path>`. Spec wants contract_id + checkpoint_id + (committed, interrupted) counts.
- F6 confirmed — `/approve` empty path at `app.rs:403-404` prints `usage:`; the enum docstring at `input/mod.rs:16-17` claims it lists active tokens. `CapabilityStore` lives in `azoth-core::authority::capability`, owned by the turn driver — the TUI pre-grants are tracked via `state.pending_approve: Option<String>` only for the NEXT token. A separate TUI-side `session_approvals: Vec<String>` is the right shape.
- F7 confirmed — no completion logic anywhere in `tui/`. Only `input/mod.rs` with `SlashCommand`. CLAUDE.md claim is aspirational. **Honest fix: trim the claim.** Implementing a full @file completion is its own sprint.
- F8 confirmed — `sheet.rs:84` uses `format!("{:?}", req.effect_class).to_lowercase()` producing `applylocal`. `EffectClass` has no `Display` impl; derives `Debug` only (`schemas/effect.rs:9`).
- F9 confirmed — `app.rs:1377-1386` sets `CardState::Aborted { reason, detail }` (canvas-persistent) AND `self.notes.push(Note::warn("aborted · ... · ..."))` (5s whisper). Same text, two surfaces.

**Scope fence:**
- Not implementing real @file completion (F7) — too large for this PR.
- Not refactoring the CapabilityStore/TUI channel to fetch live tokens (F6) — tracking TUI-local pre-grants is the honest scope.
- Not bumping ollama-side `num_ctx` plumbing (F3) — azoth-side budget is the proximate blocker; ollama num_ctx is a separate concern.
- Not implementing the spec'd persistent resume banner widget (F5) — enriching the existing note is honest, within scope.

---

## Task 1 · Preflight (branch, baseline)

**Files:**
- No files touched — pure git setup.

- [ ] **Step 1.1:** Confirm clean worktree on `main` @ `c45616f`. If unclean, stash or abort.
- [ ] **Step 1.2:** `git checkout -b feat/post-e2e-gap-fixes-f1-to-f9`
- [ ] **Step 1.3:** Baseline test suite — `cargo test --workspace 2>&1 | tail -30` — record PASS count so regressions are detectable.

---

## Task 2 · F8 (Display for EffectClass) — land first, used by F1

Doing F8 first because F1's whisper text needs the EffectClass snake_case string.

**Files:**
- Modify: `crates/azoth-core/src/schemas/effect.rs` (add `Display` impl after the `impl EffectClass` block around line 42)
- Modify: `crates/azoth/src/tui/sheet.rs:84` (replace `format!("{:?}", ...).to_lowercase()` with `req.effect_class.to_string()`)
- Test: add `#[test]` to `schemas/effect.rs` tests

- [ ] **Step 2.1:** Write the failing test in `crates/azoth-core/src/schemas/effect.rs`:

```rust
#[cfg(test)]
mod tests_display {
    use super::*;
    #[test]
    fn display_is_snake_case() {
        assert_eq!(EffectClass::Observe.to_string(), "observe");
        assert_eq!(EffectClass::Stage.to_string(), "stage");
        assert_eq!(EffectClass::ApplyLocal.to_string(), "apply_local");
        assert_eq!(EffectClass::ApplyRepo.to_string(), "apply_repo");
        assert_eq!(EffectClass::ApplyRemoteReversible.to_string(), "apply_remote_reversible");
        assert_eq!(EffectClass::ApplyRemoteStateful.to_string(), "apply_remote_stateful");
        assert_eq!(EffectClass::ApplyIrreversible.to_string(), "apply_irreversible");
    }
}
```

- [ ] **Step 2.2:** Run `cargo test -p azoth-core display_is_snake_case` → expect fail (no Display impl yet).

- [ ] **Step 2.3:** Implement `Display`:

```rust
impl std::fmt::Display for EffectClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            EffectClass::Observe => "observe",
            EffectClass::Stage => "stage",
            EffectClass::ApplyLocal => "apply_local",
            EffectClass::ApplyRepo => "apply_repo",
            EffectClass::ApplyRemoteReversible => "apply_remote_reversible",
            EffectClass::ApplyRemoteStateful => "apply_remote_stateful",
            EffectClass::ApplyIrreversible => "apply_irreversible",
        };
        f.write_str(s)
    }
}
```

- [ ] **Step 2.4:** Update `crates/azoth/src/tui/sheet.rs:84` to use the Display impl:

```rust
let effect_label = req.effect_class.to_string();
```

- [ ] **Step 2.5:** `cargo test -p azoth-core display_is_snake_case` → PASS; `cargo test -p azoth` → all sheet tests pass.

- [ ] **Step 2.6:** Commit:

```bash
git add crates/azoth-core/src/schemas/effect.rs crates/azoth/src/tui/sheet.rs
git -c user.email=dev.ungheni@gmail.com -c user.name=nalyk commit -m "azoth: F8 · snake_case Display for EffectClass (apply_local not applylocal)"
```

---

## Task 3 · F9 (abort whisper shortens, no double-print of detail)

**Files:**
- Modify: `crates/azoth/src/tui/app.rs:1382-1386`
- Test: inline in `tui/app.rs::tests` — assert the whisper note on TurnAborted is short (reason only).

- [ ] **Step 3.1:** Write failing test:

```rust
#[test]
fn turn_aborted_whisper_note_is_reason_only_not_full_detail() {
    let mut state = AppState::new(AppInit::default());
    state.handle_session_event(SessionEvent::TurnStarted {
        turn_id: TurnId::from("t_1"),
        run_id: RunId::from("r_1"),
        parent_turn: None,
        timestamp: "2026-04-24T20:00:00Z".into(),
    });
    state.handle_session_event(SessionEvent::TurnAborted {
        turn_id: TurnId::from("t_1"),
        reason: AbortReason::ContextOverflow,
        detail: Some("estimate 36072 tokens > profile max_context_tokens 32768".into()),
        usage: Usage::default(),
        at: Some("2026-04-24T20:00:05Z".into()),
    });
    // Whisper is a short hint; the canvas card carries the full detail.
    let note = state.notes.last().expect("abort note");
    assert!(note.text.starts_with("aborted · "), "got: {}", note.text);
    assert!(!note.text.contains("estimate 36072"),
        "whisper must not duplicate canvas detail; got: {}", note.text);
}
```

- [ ] **Step 3.2:** `cargo test -p azoth turn_aborted_whisper_note_is_reason_only` → fail (current impl includes detail).

- [ ] **Step 3.3:** Change `app.rs:1382-1386`:

```rust
// F9: whisper is a "here's where to look" hint; canvas card carries
// the full reason+detail. Previously we pushed reason+detail here,
// duplicating the card state line one row below it.
self.notes.push(Note::warn(format!("aborted · {reason_str}")));
```

- [ ] **Step 3.4:** `cargo test -p azoth turn_aborted_whisper_note_is_reason_only` → pass.

- [ ] **Step 3.5:** Commit:

```bash
git -c user.email=dev.ungheni@gmail.com -c user.name=nalyk commit -am "azoth/tui: F9 · shorten abort whisper, canvas owns detail"
```

---

## Task 4 · F1 (approval whisper tells the truth)

**Files:**
- Modify: `crates/azoth/src/tui/whisper.rs` — new signature `render_line(&self, theme, latest_note, pending_approval)`.
- Modify: `crates/azoth/src/tui/render.rs` — plumb `state.pending_approval` into the call.
- Test: inline in `whisper.rs::tests`.

- [ ] **Step 4.1:** Write the failing test at bottom of `whisper.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::Theme;
    use azoth_core::schemas::{ApprovalRequestMsg, ApprovalId, TurnId, EffectClass};

    #[test]
    fn pending_approval_overrides_narration() {
        let mut w = Whisper::default();
        w.set("running bash");
        let theme = Theme::detect();
        let req = ApprovalRequestMsg {
            turn_id: TurnId::from("t_1"),
            approval_id: ApprovalId::from("apv_1"),
            effect_class: EffectClass::ApplyLocal,
            tool_name: "bash".into(),
            summary: "grep -rn ...".into(),
        };
        let line = w.render_line(&theme, None, Some(&req));
        let flat: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(flat.contains("awaiting approval"), "got: {flat}");
        assert!(flat.contains("bash"), "got: {flat}");
        assert!(flat.contains("apply_local"), "got: {flat}");
        assert!(!flat.contains("running bash"),
            "must not leak prior narration when blocked on approval; got: {flat}");
    }
}
```

- [ ] **Step 4.2:** `cargo test -p azoth pending_approval_overrides_narration` → fail (wrong arity).

- [ ] **Step 4.3:** Update `whisper.rs::render_line`:

```rust
pub fn render_line(
    &self,
    theme: &Theme,
    latest_note: Option<&Note>,
    pending_approval: Option<&azoth_core::schemas::ApprovalRequestMsg>,
) -> Line<'static> {
    // F1 · the approval sheet may be off-screen under narrow terminals or
    // hidden behind scrollback; the whisper must not lie by showing the
    // prior "running <tool>" narration while the worker is blocked on the
    // user. Priority: approval > narration > note > zero-state.
    if let Some(req) = pending_approval {
        let tool = req.tool_name.clone();
        let cls = req.effect_class.to_string();
        return Line::from(vec![
            Span::raw("      "),
            Span::styled("⏸", theme.ink(Palette::AMBER)),
            Span::raw(" "),
            Span::styled("awaiting approval", theme.bold()),
            Span::styled(format!(" · {tool} → {cls}"), theme.italic_dim()),
        ]);
    }
    // …existing narration / note / zero-state branches unchanged…
}
```

- [ ] **Step 4.4:** Update the render.rs call site. Find `whisper.render_line(theme, latest_note)` and change to `whisper.render_line(theme, latest_note, state.pending_approval.as_ref())`.

- [ ] **Step 4.5:** `cargo test -p azoth pending_approval_overrides_narration` → pass. Full `cargo test -p azoth` green.

- [ ] **Step 4.6:** Commit:

```bash
git -c user.email=dev.ungheni@gmail.com -c user.name=nalyk commit -am "azoth/tui: F1 · whisper shows 'awaiting approval' when sheet is up (was lying with 'running <tool>')"
```

---

## Task 5 · F2 (/continue refuses after context_overflow)

**Files:**
- Modify: `crates/azoth/src/tui/app.rs:376-387` (PaletteAction::Continue handler).
- Test: inline.

- [ ] **Step 5.1:** Write failing test:

```rust
#[test]
fn slash_continue_refuses_after_context_overflow() {
    let mut state = AppState::new(AppInit::default());
    let tid = TurnId::from("t_1");
    state.handle_session_event(SessionEvent::TurnStarted {
        turn_id: tid.clone(),
        run_id: RunId::from("r_1"),
        parent_turn: None,
        timestamp: "2026-04-24T20:00:00Z".into(),
    });
    state.handle_session_event(SessionEvent::TurnAborted {
        turn_id: tid,
        reason: AbortReason::ContextOverflow,
        detail: Some("estimate 40000 > 32768".into()),
        usage: Usage::default(),
        at: Some("2026-04-24T20:00:05Z".into()),
    });
    state.run_palette_action(super::PaletteAction::Continue);
    assert!(
        state.pending_user_text.is_none(),
        "/continue after context_overflow must not queue continuation text"
    );
    assert!(
        state.notes.last().map(|n| n.text.as_str()).unwrap_or("").contains("context full"),
        "user-visible refusal note required"
    );
}
```

- [ ] **Step 5.2:** `cargo test -p azoth slash_continue_refuses_after_context_overflow` → fail.

- [ ] **Step 5.3:** Modify the Continue handler (app.rs:376):

```rust
PaletteAction::Continue => {
    // F2 · /continue is for `model_truncated` aborts. After a
    // `context_overflow` the context is the problem, so queuing
    // another turn just re-overflows — witnessed in 2026-04-24 E2E
    // on run_f9c7978e66de (two back-to-back overflows).
    let last_was_overflow = self.cards.last().is_some_and(|c| {
        matches!(&c.state, CardState::Aborted { reason, .. } if reason == "ContextOverflow")
    });
    if last_was_overflow {
        self.notes.push(Note::warn(
            "context full — /quit and start a fresh session, or shrink the scope",
        ));
    } else {
        self.pending_user_text = Some(
            "Please continue from where you left off — pick up the \
             partial output and finish."
                .to_string(),
        );
        self.notes.push(Note::info("continue requested"));
    }
}
```

- [ ] **Step 5.4:** `cargo test -p azoth slash_continue_refuses_after_context_overflow` → pass.

- [ ] **Step 5.5:** Commit.

---

## Task 6 · F3 (bump ollama profile context to 131 072)

**Files:**
- Modify: `crates/azoth/src/tui/config.rs:42,52` (2 × `32_768 → 131_072`)
- Modify: `crates/azoth-core/src/adapter/profile.rs:98,117` (2 × `32_768 → 131_072`)
- Test: modify the existing `ollama_anthropic_uses_content_block_shape` test at `profile.rs:149` and sibling to assert the new budget, add explicit assertion.

- [ ] **Step 6.1:** Write assertion in the existing `ollama_anthropic_uses_content_block_shape` test body:

```rust
assert_eq!(p.max_context_tokens, 131_072,
    "ollama profiles pinned at 131072 per F3 2026-04-24: 32k saturates after 3 turns");
```

- [ ] **Step 6.2:** Run; expect fail (still 32k).

- [ ] **Step 6.3:** Apply the four one-line edits at:
  - `crates/azoth-core/src/adapter/profile.rs:98` — `max_context_tokens: 131_072`
  - `crates/azoth-core/src/adapter/profile.rs:117` — same
  - `crates/azoth/src/tui/config.rs:42` — same
  - `crates/azoth/src/tui/config.rs:52` — same

- [ ] **Step 6.4:** Tests pass. Add a 2-line comment at `profile.rs:90-92` noting the 2026-04-24 E2E observation.

- [ ] **Step 6.5:** Commit.

---

## Task 7 · F4 (wall-clock restored on resume)

**Files:**
- Modify: `crates/azoth/src/tui/app.rs:1050` (TurnStarted) and `:1337` (TurnCommitted) to read timestamps.
- Modify: `crates/azoth/src/tui/card.rs` — add `TurnCard::agent_at(turn_id, started_wall)` alt ctor.
- Helper: inline parser using `time::OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339)` → `SystemTime::from(...)`.
- Test: inline test asserting post-resume card carries the original `started_wall` / `committed_wall`.

- [ ] **Step 7.1:** Write failing integration-style test in `crates/azoth/src/tui/app.rs` tests:

```rust
#[test]
fn resume_hydrates_wall_clocks_from_turn_started_and_turn_committed_timestamps() {
    let mut state = AppState::new(AppInit::default());
    let start_iso = "2026-04-24T20:00:00Z";
    let commit_iso = "2026-04-24T20:00:42Z";
    state.handle_session_event(SessionEvent::TurnStarted {
        turn_id: TurnId::from("t_1"),
        run_id: RunId::from("r_1"),
        parent_turn: None,
        timestamp: start_iso.into(),
    });
    let card = state.cards.last().expect("card");
    let started_wall = card.started_wall;
    // Must reflect the 2026-04-24 event time, not process start.
    let expected_started = humantime::parse_rfc3339(start_iso).unwrap();
    assert_eq!(started_wall, expected_started,
        "TurnStarted must hydrate card.started_wall from event timestamp");
    state.handle_session_event(SessionEvent::TurnCommitted {
        turn_id: TurnId::from("t_1"),
        outcome: CommitOutcome::Success,
        usage: Usage::default(),
        user_input: None,
        final_assistant: None,
        at: Some(commit_iso.into()),
    });
    let card = state.cards.last().unwrap();
    let cw = card.committed_wall.expect("committed_wall set");
    let expected_commit = humantime::parse_rfc3339(commit_iso).unwrap();
    assert_eq!(cw, expected_commit,
        "TurnCommitted.at must overwrite card.committed_wall");
    // Derived: elapsed should be ~42s
    let elapsed = cw.duration_since(started_wall).unwrap();
    assert_eq!(elapsed.as_secs(), 42);
}
```

- [ ] **Step 7.2:** Add a small parse helper at top of app.rs (or in a utility mod):

```rust
fn parse_rfc3339_to_system_time(s: &str) -> Option<std::time::SystemTime> {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::parse(s, &Rfc3339)
        .ok()
        .map(std::time::SystemTime::from)
}
```

(Use `time` if already in deps; else `humantime::parse_rfc3339` — check Cargo.toml.)

- [ ] **Step 7.3:** Update `TurnStarted` handler:

```rust
SessionEvent::TurnStarted { turn_id, timestamp, .. } => {
    let wall = parse_rfc3339_to_system_time(&timestamp)
        .unwrap_or_else(std::time::SystemTime::now);
    let mut card = TurnCard::agent(turn_id.to_string());
    card.started_wall = wall;
    self.cards.push(card);
    // … rest unchanged
}
```

- [ ] **Step 7.4:** Update `TurnCommitted` handler:

```rust
SessionEvent::TurnCommitted { turn_id, usage, at, .. } => {
    // … existing body …
    card.committed_wall = at
        .as_deref()
        .and_then(parse_rfc3339_to_system_time)
        .or(Some(std::time::SystemTime::now()));
}
```

- [ ] **Step 7.5:** Tests pass. Commit.

---

## Task 8 · F5 (resume banner enriched)

**Files:**
- Modify: `crates/azoth/src/tui/app.rs:2341-2348`.
- Test: inline assertion that the resume note contains contract + checkpoint + turn counts.

- [ ] **Step 8.1:** Test shape (needs `resume_scan` fixture):

```rust
#[test]
fn resume_banner_carries_contract_checkpoint_turn_counts() {
    // Build a tiny in-memory scan with 1 contract_accepted, 1 checkpoint,
    // 2 turn_committed, 1 turn_interrupted; call the banner formatter.
    let summary = super::resume_summary(/*contract_id=*/ Some("ctr_abc"),
        /*checkpoint_id=*/ Some("chk_xyz"),
        /*committed=*/ 2,
        /*interrupted=*/ 1);
    assert_eq!(summary, "resumed · ctr_abc · chk_xyz · 2 turns · 1 interrupted");
}
```

- [ ] **Step 8.2:** Refactor the banner into a pure helper function `resume_summary(...)` reachable from the test.

- [ ] **Step 8.3:** Populate it from `resume_scan.last_contract()`, `resume_scan.last_checkpoint()`, `turns_completed`, and a new `turns_interrupted` derivation from `scan.outcomes`.

- [ ] **Step 8.4:** Test passes; commit.

---

## Task 9 · F6 (/approve lists TUI-side session grants)

**Files:**
- Modify: `crates/azoth/src/tui/app.rs` — add `session_approvals: Vec<String>` to `AppState`; mutate in `PaletteAction::Approve(Some(..))` + on `ClickTarget::SheetApproveSession` grant.
- Modify: `handle_slash::Approve(None)` / `PaletteAction::Approve(None)` — render the list.
- Test: inline.

- [ ] **Step 9.1:** Failing test:

```rust
#[test]
fn slash_approve_empty_lists_prior_session_grants() {
    let mut state = AppState::new(AppInit::default());
    state.run_palette_action(super::PaletteAction::Approve(Some("fs_write".into())));
    state.run_palette_action(super::PaletteAction::Approve(Some("bash".into())));
    // Trigger /approve with no arg
    state.run_palette_action(super::PaletteAction::Approve(None));
    let n = state.notes.last().expect("note");
    assert!(n.text.contains("fs_write") && n.text.contains("bash"),
        "list must include granted tools; got: {}", n.text);
}
```

- [ ] **Step 9.2:** Fails (current path only prints usage).

- [ ] **Step 9.3:** Add `session_approvals: Vec<String>` to `AppState`; push on each `Approve(Some(..))` (dedupe). Rewrite `Approve(None)` arm:

```rust
PaletteAction::Approve(None) => {
    if self.session_approvals.is_empty() {
        self.notes.push(Note::help("usage: /approve <tool_name>"));
    } else {
        let list = self.session_approvals.join(", ");
        self.notes.push(Note::info(format!("session-approved: {list}")));
    }
}
```

- [ ] **Step 9.4:** Also update the SheetApproveSession click handler (app.rs:604ish) to push into `session_approvals` when the sheet grants — so a sheet-grant shows up in the list next `/approve`.

- [ ] **Step 9.5:** Tests pass. Commit.

---

## Task 10 · F7 (honest CLAUDE.md trim — no aspirational claims)

**Files:**
- Modify: `CLAUDE.md` — remove `@file completion` from the input/ row, replace with literal truth.

- [ ] **Step 10.1:** Edit the row so it reads `Slash parser, history nav` instead of `Slash parser, @file completion, history nav`.

- [ ] **Step 10.2:** Add a new line under "Known deferred — DO NOT 'fix' without a dedicated round":

```
- **@file completion widget** — ergonomics favour it (type `@`, fuzzy-match
  repo files, Tab to accept). Needs its own sprint: trigger detection on
  `@`, popup widget drawing, fuzzy matcher on the `ignore::WalkBuilder`
  walk results, Tab/arrow/Esc handling. Not shipped — documented under
  F7 of the 2026-04-24 E2E findings. Re-open with a plan, not a one-off.
```

- [ ] **Step 10.3:** Commit — no code, doc-only.

---

## Task 11 · Verification

- [ ] **Step 11.1:** `cargo fmt --check` — green.
- [ ] **Step 11.2:** `cargo clippy --workspace --all-targets -- -D warnings` — green.
- [ ] **Step 11.3:** `cargo test --workspace` — all 800+ tests green, including the 9 new ones.
- [ ] **Step 11.4:** Adversarial self-review per `feedback_adversarial_self_review_before_push`: grep for `.to_string()` on `&'static str` spans in my diff; check sibling sites for each fix; re-read F1 whisper override for priority-ordering against narration/note/zero-state; confirm F4 parse_rfc3339 falls back on parse failure so old sessions don't panic.
- [ ] **Step 11.5:** Re-run the tmux E2E smoke (not full) — one prompt, confirm approval whisper reads "awaiting approval", abort whisper doesn't double-print, resume banner enriches.

---

## Task 12 · PR open

- [ ] **Step 12.1:** `git push -u origin feat/post-e2e-gap-fixes-f1-to-f9`
- [ ] **Step 12.2:** `gh pr create --base main --head feat/post-e2e-gap-fixes-f1-to-f9` with body listing F1..F9, each with "before / after / test".

---

## Self-review checklist (per writing-plans skill)

- **Spec coverage**: F1 (Task 4), F2 (Task 5), F3 (Task 6), F4 (Task 7), F5 (Task 8), F6 (Task 9), F7 (Task 10), F8 (Task 2), F9 (Task 3). All 9 mapped.
- **Placeholders**: none — every step carries exact code.
- **Type consistency**: `ApprovalRequestMsg` used in F1 test must be the real schema type; `resume_summary` helper must return String; `session_approvals: Vec<String>` consistent across Task 9 uses.
- **Order dependencies**: F8 before F1 (F1 uses EffectClass::to_string). F3 independent. F4 needs time/humantime crate check. Tasks 2–10 can commit in any order after preflight; I'll do them in numbered order.
