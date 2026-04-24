# azoth

<critical_mandatory_rule>
Don't flatter me. Use radical candor when you communicate with me. Tell me something I need to know even if I don't want to hear it
</critical_mandatory_rule>

Contract-centric, event-sourced, provider-agnostic coding agent runtime.
Rust workspace (three crates):

- `azoth-core` — library, zero frontend coupling, zero heavy indexing deps.
- `azoth-repo` — v2 indexer plane: FTS5 (`FtsLexicalRetrieval`), tree-sitter
  symbol index (`SqliteSymbolIndex`), co-edit graph (`CoEditGraphRetrieval`),
  TDAD impact selector (`CargoTestImpact`). Depends on `azoth-core`.
- `azoth` — CLI/TUI binary. Depends on both. Linux only.

Dependency arrow is strictly one-way: `azoth → azoth-repo → azoth-core`.

Full architecture specs: @docs/draft_plan.md (v1 skeleton), @docs/v2_plan.md (repo intelligence moat).

## Commands

```bash
source "$HOME/.cargo/env"          # required — cargo is NOT on default PATH
cargo check --workspace
cargo build --workspace
cargo test --workspace             # 330+ tests, unit + integration
cargo test -p azoth-core           # core crate only
cargo clippy --workspace -- -D warnings
cargo fmt --check
AZOTH_PROFILE=anthropic cargo run  # or: ollama-qwen-anthropic (default), openai, openrouter
```

## Eight Invariants

These are runtime laws. Code that violates any invariant is a bug regardless of whether it compiles.

1. **Transcript is not memory** — ContextKernel recompiles from durable state every turn
2. **Deterministic controls outrank model output** — AuthorityEngine has final say
3. **Every non-trivial run has a contract** — explicit goals + success criteria
4. **Every side effect has a class** — EffectClass enum (Observe, Stage, ApplyLocal, ApplyRepo, ApplyRemote*, ApplyIrreversible)
5. **Every run leaves structured evidence** — SessionEvents, checkpoints, artifacts
6. **Every subsystem is eval-able** — telemetry emits measurable signals
7. **Turn-scoped atomicity** — TurnStarted must be followed by exactly one of TurnCommitted / TurnAborted / TurnInterrupted
8. **Time is taint, not preface** (Chronon Plane, v2.0.2) — every persisted timestamp flows through an injected `Clock` (production: `SystemClock`; tests: `FrozenClock`; replay: `VirtualClock`); every externally-observed fact the model sees can carry `(observed_at, valid_at)`; contracts may bound wall-clock spend via `scope.max_wall_secs`; open turns emit `TurnHeartbeat` at a throttled cadence. Time never enters the constitution lane as a frozen string.

## Architecture Constraints

- `azoth-core` has ZERO frontend deps AND zero heavy indexer deps. Never import ratatui, crossterm, clap, tui-textarea, tree-sitter, or fts-specific crates in azoth-core. Heavy indexing lives in `azoth-repo`.
- `azoth-repo` houses tree-sitter, FTS5 (rusqlite `bundled` already includes it — no `fts5` feature flag needed), git shell-out, and TDAD backends. It depends on `azoth-core` for traits (`LexicalRetrieval`, `SymbolRetrieval`, `GraphRetrieval`, `ImpactSelector`) and schema types.
- Internal model protocol uses **Anthropic Messages content-block shape**. The OpenAI adapter downcasts on the wire. Do not introduce a third internal format.
- `schemas/` is the type hub — changes ripple everywhere. Treat it as a stability boundary.
- Every Tool impl must: (a) define a typed `Input` struct, (b) declare an `EffectClass`, (c) go through the taint gate via `ErasedTool` blanket. Never bypass the dispatcher.
- `Origin` enum (taint provenance): `User`, `Contract`, `ToolOutput`, `RepoFile`, `WebFetch`, `ModelOutput`, **`Indexer`** (v2 — FTS5/symbol/graph). Tools declare `permitted_origins()`; full policy DSL enforcement ships in v2.5.
- ContextKernel 5-lane ordering is **cache-prefix-stable**: constitution → working_set → evidence → checkpoint → exit_criteria. Never reorder. Within evidence, composite lanes tag items (`graph`, `symbol`, `lexical`, `fts`) and the stable sort + reranker MUST preserve byte-stability across reindexes — FTS snippets pass through `normalize_snippet` before landing in `inline`.
- **JSONL is authoritative** (CRIT-1). SQLite mirror is a rebuildable secondary index. Never write to SQLite as primary store. `.azoth/state.sqlite` is shared between SqliteMirror, RepoIndexer, FtsLexicalRetrieval, SqliteSymbolIndex, CoEditGraphRetrieval — each opens its own `rusqlite::Connection`; WAL mode is persisted on the file.
- Event lifecycle: TurnStarted → exactly one terminal marker. No orphaned events, no silent returns.
- Tiers C and D (`apply_remote_*`, `apply_irreversible`) return `EffectNotAvailable` in v1/v2. Do not implement real dispatchers.
- Sandbox is Linux-only: Tier A (user namespaces + Landlock + seccomp), Tier B (+ fuse-overlayfs).
- v2 retrieval defaults: `retrieval.mode = composite`, `retrieval.lexical_backend = fts`. Pre-v2 single-lane ripgrep behaviour stays reachable via `AZOTH_RETRIEVAL_MODE=legacy` / `AZOTH_LEXICAL_BACKEND=ripgrep` for forensic comparisons.

## TUI (PAPER) UI/UX Architecture

The interactive TUI lives in `crates/azoth/src/tui/`. Design system: **PAPER** — typography is the hero, one accent (cyan-74), minimal chrome, motion used for *state signalling*, not decoration. Every edit below the line must preserve (a) the cache-discipline invariants, (b) the dual-clock pattern, (c) the modal click-gating allowlist, and (d) the `Line<'static>` borrow shape. Breaking any of them silently shows up as a corrupted render, a panic under narrow terminals, or a memory-pressure regression under long sessions — not as a compile error.

### File map

| File | Role |
|---|---|
| `app.rs` | `AppState`, biased `tokio::select!` loop, event dispatch (input / mouse / authority / tool / model), session-event → state mutator, ClickTarget handlers. Owns cards, click_map, focus/scroll, all overlay states. |
| `render.rs` | `frame()` orchestrator. `render_canvas()` two-pass virtualisation. click_map rebuild (outer Vec resized only on terminal-height change; inner Vecs cleared in place — R24 fix). Draws sheet / palette / inspector / rail / whisper / splash over canvas. |
| `card.rs` | `TurnCard` + `ToolCell` + `CardState` + ALL per-card caches. Atomic visual unit; the canvas is `Vec<TurnCard>`. |
| `markdown.rs` | `render()` via pulldown-cmark with GFM extensions. `tint_code()` minimal syntax tinting. `render_table()` whitespace-aligned GFM tables. `pad_to()` width-safe padder with `width=0` guard. |
| `theme.rs` | `Palette` (one ACCENT + AMBER + ABORT + ink ladder + R27 semantic constants). `GlyphPair` + `Theme::glyph()` + `Theme::detect()` via POSIX `LC_ALL > LC_CTYPE > LANG` precedence; `TERM=dumb` forces ASCII. |
| `motion.rs` | Animation primitives: `pulse_phase`, `spinner_frame`, `sweep_frame`, `typing_frame`, `shimmer_spans`, `bloom_decay`. Return `&'static str` — NEVER allocate. |
| `palette.rs` | ⌃K command palette. `STATIC_ENTRIES` const + contextual (jump / approve / contract). `cached_entries: (query, turn_count, Vec)` — render runs at 60fps while open; cache prevents recompute. |
| `sheet.rs` | Approval modal. `upper.max(9)` floor prevents `clamp` panic on narrow terminals. Click-map registration via `render()` param. |
| `inspector.rs` | ⌃2 right drawer. `InspectorData { ctx_pct, ctx_history, packet_digest, contract_goal, contract_budget, evidence_lanes, tools, turn_id }`. `render_section_header` takes `&'static str` (all call sites literals). |
| `rail.rs` | ⌃1 left drawer. Turn miniatures — role + ts + prose excerpt per card. |
| `whisper.rs` | Single-row narrator above composer. Three states: narrating (spinner + text + elapsed), recent note (<5s old), default "ready · ⌃K for commands". |
| `input/` | `tui-textarea-2 0.10` wrapper (for ratatui 0.30 — DO NOT use `tui-textarea 0.7`). Slash parser, history nav. (@file completion is NOT wired — see Known deferred below.) |

### Render loop (app.rs)

```rust
loop {
    tokio::select! {
        biased;                                  // LOAD-BEARING. Do not reorder.
        Some(_) = cancel.recv()       => { /* Ctrl+C — highest priority */ }
        Some(ev) = input_rx.recv()    => state.handle_input(ev),
        Some(ev) = authority_rx.recv()=> state.handle_authority(ev),
        Some(ev) = tool_rx.recv()     => state.handle_tool(ev),
        Some(ev) = model_rx.recv()    => state.handle_model(ev),
        _        = ticker.tick()      => { /* dirty-gated via has_active_animation() */ }
    }
    if state.dirty { terminal.draw(|f| render::frame(f, &mut state))?; state.dirty = false; }
}
```

- `biased;` + cancellation first is **non-negotiable** — reordering reintroduces the MED-3 Ctrl+C starvation bug under model-stream flood.
- Dedicated input task reads `crossterm::EventStream` → bounded(128) channel. Prevents keyboard events being starved by a runaway token stream.
- Channel sizes are load-bearing: input 128 (keyboard bursts), model 64 (matches adapter bound, see Adapter Protocol), tool 32, authority 8.
- 50ms ticker — chosen to alias cleanly with the 80ms spinner cadence. `has_active_animation()` gates `dirty = true`, so idle sessions pay ~0 redraws while live animations never freeze.

### Canvas virtualisation (render.rs:render_canvas)

Two-pass shape:

1. **Pass 1 — estimate.** `est_total = Σ est_h(card.last_rendered_rows)` over `visible_indices`. Derives `est_target_top` and `est_target_bot` for the viewport window.
2. **Pass 2 — visible slice.** Walk cards; for each, test if `[cursor_y, cursor_y + est_h)` intersects viewport. Off-screen cards: skip entirely (advance cursor_y). On-screen: `render_rows()` and copy into output. Once `cursor_y >= est_target_bot`: `break` (never `continue` — R22 fix, the old `continue` walked remaining 9990 cards on a 10k session).
3. `first_card_local_skip` is the "how many rows of the first intersecting card fall above viewport" value. Passed to `Paragraph::scroll((skip, 0))` so ratatui crops the top.

`click_map: Vec<Vec<(Range<u16>, ClickTarget)>>` — outer Y-indexed (resized only on terminal-height change; inner cleared in place per frame), inner holds all buttons on that row with their X range. **Cards and side drawers share Y-coordinates**, so card click-map entries must be constrained to `area.x..(area.x + area.width)` or they leak through Rail / Inspector (R10 fix).

### Cache discipline (card.rs)

Every cache has a **key** (all inputs that affect the computed output) and a **mutator list** (every method that can flip any key input). Missing a key ⇒ stale render diverges from state. Missing an invalidator ⇒ same.

| Cache | Key | Invalidated by |
|---|---|---|
| `TurnCard.cached_prose` | `(prose_revision, theme.unicode)` | `append_prose()` bumps `prose_revision` |
| `TurnCard.cached_thoughts` | `thoughts_revision` | `append_thought()` bumps `thoughts_revision` |
| `TurnCard.cached_header` (R27) | `(unicode, ts_bucket, usage, state_frozen)` | 1Hz ts tick (live cards), state transition (live → terminal), usage update, theme flip |
| `ToolCell.cached_preview_render` | `Option<Vec>; None=dirty` | `set_preview_lines()` clears |
| `ToolCell.cached_full_render` | same | `set_full_lines()` clears |
| `ToolCell.cached_header_parts` (R27) | `(unicode, expanded, has_content, name, summary)` | `set_preview_lines()` / `set_full_lines()` clear (flips `has_content`) |
| `PaletteState.cached_entries` | `(query, turn_count)` | `open()` / `close()` clear; render-time key mismatch recomputes |

**Adding a new cache field:** update the `None` default in EVERY constructor. For TurnCard there are ≥3 sites (`user()`, `agent()`, test helpers in `app.rs::tests`). For ToolCell there are ≥5 (`card.rs` test sites + `app.rs::tests` + the `ToolUse` event handler). Missing one = the cache starts as whatever `#[derive(Default)]` gave it (usually compile-error if the field isn't Default, else a hidden bug).

**Regression-test shape:** populate cache → stamp a sentinel value into the cache struct → re-render with same inputs → assert sentinel survives → change one key input → re-render → assert sentinel is evicted. Pattern replicated by every existing cache test (`cached_prose_invalidates_on_unicode_flip`, `cached_card_header_survives_same_second_rerender`, etc.).

### Dual-clock fields (R27 pattern)

Every card holds TWO clock pairs:

```rust
pub started: Instant,                        // monotonic — animation
pub started_wall: SystemTime,                // wall-clock — display + resume
pub committed_at: Option<Instant>,           // monotonic — bloom animation
pub committed_wall: Option<SystemTime>,      // wall-clock — frozen display on terminal
```

Rules:
- Animation timing (bloom, shimmer, spinner, pulse phase) ALWAYS uses `Instant`. Never `SystemTime` — it can jump backward on DST/NTP.
- Display timestamps ("t+Xs") ALWAYS use `SystemTime`. Never `Instant` — it resets on process restart, so every historical card post-resume would read "t+0.0s".
- On commit, set BOTH `committed_at = Some(Instant::now())` AND `committed_wall = Some(SystemTime::now())`.
- Display-elapsed subtraction wraps `duration_since` with `.unwrap_or_else(|e| e.duration())` — wall-clock can go backward, never panic.

This isn't cosmetic. The mono-only path breaks resume; the wall-only path corrupts animation math. See `pattern_dual_clock_for_resume_stable_tui.md` in auto-memory.

### Theme / Palette / Glyphs

- **One accent.** Never add a second hue. If you need distinction, use weight / italic / underline / bar-style.
- Semantic constants on `Palette` (use these; never hardcode `Color::Indexed(N)`): `ACCENT`, `AMBER`, `ABORT`, `INK_0..INK_4`, `CODE_BG`, `SYNTAX_STRING`, `SYNTAX_NUMBER`, `DIFF_ADD`, `DIFF_DEL`, `SHIMMER_TAIL`.
- `GlyphPair::new(unicode, ascii)` + `Theme::glyph()` dispatches on `theme.unicode`. Every glyph HAS a fallback; `TERM=dumb` or non-UTF-8 locale → ASCII. Do not add Unicode-only glyphs.
- `Theme::detect()` POSIX precedence: `LC_ALL > LC_CTYPE > LANG`. Reversing this order silently mis-detects in every `sudo`'d shell, CI container, or `LANG=en_US.UTF-8 LC_ALL=C` environment (R11 fix).

### Motion

- Phase primitives return `&'static str`. NEVER `.to_string()` them — the `Span::styled` sweep rule below applies.
- `motion::pulse_phase(elapsed_ms, period)` is the single source of truth for pulses. Do NOT read `Instant::now()` inside `render_rows`. Exceptions: commit-bloom decay + shimmer age, both with documented age windows.
- Cadence: 80ms per animation step (spinner frame, pulse half-period). Ticker at 50ms sub-divides cleanly.

### ClickTarget + modal gating invariant

```rust
pub enum ClickTarget {
    ThoughtsToggle { card_idx }, CellToggle { card_idx, cell_idx },    // canvas
    SheetApproveOnce, SheetApproveSession, SheetDeny,                   // sheet modal
    PaletteOpen, FocusToggle, RailToggle, InspectorToggle,              // status-row buttons
    JumpToTurn(usize),                                                  // rail
}
```

**Modal gating (R8 invariant).** When `palette.open` or `pending_approval.is_some()`, `handle_mouse` MUST filter the dispatched `ClickTarget` to a modal-only allowlist:

```rust
// allowlist, not denylist — new canvas targets shouldn't need to opt out
matches!(target,
    ClickTarget::SheetApproveOnce | ClickTarget::SheetApproveSession | ClickTarget::SheetDeny
    | ClickTarget::PaletteOpen)
```

Denylist style breaks the invariant every time a new canvas target is added. Reject suggestions to invert it.

### Static-str → Span::styled (R24/R26 sweep rule)

Anti-pattern: `Span::styled("text".to_string(), style)` with a `&'static str` literal. Allocates one heap `String` per frame per site. At 60fps × 100 visible cards × 5 sites/card = 30k needless allocations/sec.

Pattern: `Span::styled("text", style)` — `Cow::Borrowed`, zero alloc.

Caveats:
- A struct field `String` (e.g. `cell.name`) that must outlive the borrow still requires `.clone()` for `Line<'static>` (see "Known deferred / RefCell prose refactor" below for the full fix).
- Methods returning `&'static str` (e.g. `theme.glyph()`, `motion::spinner_frame()`) → pass directly, NEVER `.to_string()`.
- Mixed-branch conditionals (one arm literal, one arm `format!()`) → explicit `Cow<'static, str>`:
  ```rust
  let marker: Cow<'static, str> = if selected {
      Cow::Owned(format!("{} ", theme.glyph(CHEVRON)))
  } else {
      Cow::Borrowed("  ")
  };
  ```

This pattern recurred across 8+ review rounds on PR #15. Sweep recipe before any render-perf commit:
```bash
rg 'Span::(styled|raw)\("[^"]+"\.to_string\(\)' crates/azoth/src/tui/
```

### Markdown rendering (markdown.rs)

- `pulldown_cmark::Options`: `ENABLE_TABLES | ENABLE_STRIKETHROUGH | ENABLE_TASKLISTS`.
- `in_blockquote` → `End(Tag::Paragraph)` emits `Line::from(Span::styled("│ ", ACCENT))` (not empty Line) to keep the rail continuous between paragraphs (R27 fix).
- `list_depth > 0 && current_spans.is_empty()` at `Start(Tag::Paragraph)` → push `"  ".repeat(list_depth)` indent so multi-paragraph list items don't flush-left (R27 fix).
- `tint_code` is a byte-level tokenizer; `.get(start..i).unwrap_or("")` handles mid-codepoint slicing safely (no panic). Multi-byte identifiers fall through to plain ink — acceptable for minimal TUI.
- `pad_to(s, 0)` MUST return `String::new()` (R24 fix; pre-fix returned `"…"` which overran collapsed columns).
- `render_table` uses `const GAP_STR: &str = "  "`, not `" ".repeat(GAP)` + `gap.clone()` (R26 fix).

### Composer / input

- Textarea crate: `tui-textarea-2 0.10` for ratatui 0.30. `tui-textarea 0.7` targets ratatui 0.29 — DO NOT substitute.
- Slash parser lives in `input/mod.rs`, not `app.rs`. `handle_slash` → `run_palette_action` for every variant that has a palette equivalent (R14 unified pattern — prevents drift between keyboard and palette code paths; the one-time drift left `/continue` silent while the palette version showed a note).
- `Shift+Enter` = newline (via `tui-textarea-2` default); `Enter` alone sends.

### Ctrl+C / Ctrl+D cancellation wiring

- `AppState.active_cancel: Arc<Mutex<Option<CancellationToken>>>` is the
  shared per-turn cancellation handle. The worker sets `Some(token)` before
  every `drive_turn` (passes the same token into `ExecutionContext::builder`
  via `.cancellation(token)`) and clears to `None` after the future
  resolves, regardless of outcome.
- Ctrl+C handler (`app.rs::handle_key`, the `KeyCode::Char('c')` arm): if
  `active_cancel` holds `Some(token)`, calls `token.cancel()` + pushes a
  whisper-adjacent note "cancelling turn…", does NOT flip `should_quit`.
  If `None`, falls through to `should_quit = true` (legacy idle quit).
- Ctrl+D is unconditional quit — the escape hatch, does NOT route
  through the cancel branch. This lets a user always exit even if the
  worker is wedged and the cancel path is stuck.
- Core side does the rest: `TurnDriver::drive_turn` polls
  `ctx.cancelled()` pre-invoke (`turn/mod.rs:545`) and mid-stream
  (`:669`), emitting `TurnInterrupted { reason: UserCancel, partial_usage }`
  through the JSONL writer. Preserves streamed output tokens in the
  `partial_usage` field — crash-recovery synthetic on resume would lose them.
- Regression tests: three unit tests in `app.rs::tests::ctrl_c_*` +
  `ctrl_d_always_quits_even_mid_turn`, plus the existing
  `tests/abort_preserves_streamed_usage.rs::user_cancel_interrupt_preserves_streamed_output_tokens`
  integration test on the core side. Both sides are green.

### Session events → TUI state updates

The worker task (`app.rs::handle_session_event`) is the only writer to visible state. Event → mutation mapping:

| SessionEvent | TUI effect |
|---|---|
| `TurnStarted` | `cards.push(TurnCard::agent(turn_id))`; whisper narration start |
| `ContentBlockText` | `card.append_prose(text)` → bumps `prose_revision` |
| `ContentBlockThinking` | `card.append_thought(text)` |
| `ContentBlockToolUse` | `card.add_cell(ToolCell { ... })` |
| `ToolResult` | `cell.set_preview_lines() + set_full_lines() + result = Ok/Err` |
| `PendingApproval` | `pending_approval = Some(req)`; `card.state = AwaitingApproval` |
| `TurnCommitted` | `state=Committed`, `usage=Some(chip)`, `committed_at + committed_wall = Some(now)`; `committed_turns += 1` |
| `TurnAborted` | `state = Aborted { reason, detail }` |
| `TurnInterrupted` | `state = Interrupted { reason }` |
| `RetrievalQueried` | `inspector_data.evidence_lanes.push((backend, label))` |
| `SymbolResolved` | `inspector_data.evidence_lanes.push(("symbol", label))` |

`inspector_data.evidence_lanes.clear()` on TurnStarted so the inspector shows *this turn's* retrieval, not residue (R1 fix).

### Debugging

- `tracing` fmt to stderr CORRUPTS the alternate-screen TUI. Redirect to file or use a gated feature. Never `println!` / `eprintln!`.
- Tests: `cargo test -p azoth tui -- --test-threads=1`. Parallel tests interfere with render helpers (TempDir drop order, env-var races — `backpressure_smoke` specifically flakes at high parallelism).
- Headless TUI tests use `MockAdapter` + `MockScript` from `azoth-core` — deterministic stream, no network.
- If a render diverges from state, the first thing to check is CACHE invalidation — did the mutator path clear the right cache? Regression tests for this are named `X_invalidates_on_Y_change`.

### Extension recipes

**New widget:**
1. Module under `tui/`. Pure `pub fn render(f, area, &state, &theme)` (or `&mut state` if it owns a cache).
2. Wire into `render::frame()`. Draw order: widgets on top draw last.
3. Interactive: add `ClickTarget` variant; register with explicit X-range in `click_map` during render; handle in `handle_click_target`.
4. Modal: add variant to the modal-active allowlist in `handle_mouse`.

**New slash command:**
1. Variant in `SlashCommand` (`input/mod.rs`).
2. Parse in input parser.
3. Delegate `handle_slash` → `run_palette_action` for the shared variant (R14 pattern).
4. Add to `palette::STATIC_ENTRIES` for discoverability.

**New session-event → UI update:**
1. Tap `SessionEvent::X` in `app.rs` worker handler.
2. Update state.
3. Invalidate the relevant cache (or bump revision) if any key input changed.
4. `state.dirty = true`.

**New cache field:**
1. Add to struct with a doc comment naming the key + invalidation trigger.
2. Init `None` (or equivalent default) in EVERY constructor — grep ruthlessly.
3. Add invalidation in every mutator that can change a key input.
4. Regression test: sentinel survives same-inputs rerender, evicted on key change.

### Known deferred — DO NOT "fix" without a dedicated round

Future Claude: these have been examined multiple times. Each is tracked in-code with scope. Attempting a half-fix creates API churn. Do the full refactor in its own round or leave alone.

- **card.rs:562 prose span clone** (gemini HIGH, raised 4×). The `Cow::Owned(String)` clone per line per frame for prose. Full fix requires: (a) wrap `cached_prose`, `cached_thoughts`, `last_rendered_rows`, `cached_header` in `RefCell`/`Cell`; (b) `render_rows(&self, ...)` with interior mutability; (c) propagate `'card_ref` lifetime through `render_canvas` so spans borrow rather than clone; (d) unified `push_row!` macro for the ~30 push sites inside `render_rows`. Shares (d) with within-card virtualisation — land both together.
- **render.rs:259 est_total O(N)** (raised 3×). Comment-accepted at <100 cards; typical sessions. Reopen only if a real workload demonstrates the wall.
- **render.rs:317 within-card virtualisation** — cards taller than remaining viewport materialise rows that get cropped. Shares emission refactor with RefCell round.
- **card.rs:512 render_rows Vec-return signature** — shares refactor.
- **tint_code multi-byte** — byte-tokenizer is UTF-8 safe; multi-byte identifiers fall through to plain ink. Regression test `tint_code_handles_mixed_ascii_and_multi_byte_content` covers no-panic + no-dropped-chars. Acceptable as-is.
- **@file completion widget** (F7 2026-04-24 dogfood) — ergonomics favour it: type `@`, fuzzy-match repo files against `ignore::WalkBuilder` results, Tab to accept. This earlier `input/` row claimed the feature existed; it never did. Implementing it properly needs a full sprint: trigger detection on `@` inside `tui-textarea-2` edits, popup widget drawing with `render::frame` z-order above composer, fuzzy matcher + caching, Tab / Arrow / Esc nav, pending-token render inside the composer, and test coverage for each of those. Model-side workaround today: users type `@path` as literal text; the model can still act on it via `repo_read_file`. Do NOT half-ship.

## Test Patterns

- Integration tests live in `crates/azoth-core/tests/`, unit tests inline with `#[cfg(test)]`
- Test helpers returning `PathBuf` from `TempDir` MUST also return the `TempDir` — drop order deletes the directory before assertions otherwise
- Stream/SSE parsers: test with 17-byte chunk splits to catch mid-boundary bugs
- Use `MockAdapter` + `MockScript` for headless TurnDriver tests, `wiremock` for live HTTP adapter tests
- All test I/O uses `tempfile` crate — never write to fixed paths

## Gotchas

- IMPORTANT: The TurnDriver `tokio::select!` is **biased** with cancellation branch first. Moving it causes Ctrl+C starvation under stream flood (MED-3 regression). Do not reorder.
- IMPORTANT: Adapter `invoke()` pushes to a bounded(64) mpsc channel. The driver MUST drain concurrently or deadlock occurs on long responses.
- Contract defaults: `max_turns=32`, `max_apply_local=20`, `max_apply_repo=5` (hardcoded in `contract::draft()`)
- ID types (`TurnId`, `RunId`, `ContractId`, etc.) are newtype wrappers around `String`. Use `::new()` for UUIDs, `::from("literal")` in tests.
- Logging: `tracing` crate only, never `println!` or `eprintln!` — TUI owns stdout/stderr via alternate screen.

## Workflow

- IMPORTANT: ALWAYS use mcp__filesystem-with-morph__edit_file tool to make any code edits. Do not use the default edit tool.
- Execute autonomously end-to-end. Do not present option menus or ask "would you like me to..."
- Run `cargo fmt`, `cargo clippy`, `cargo test --workspace` before declaring any task complete.
- Git identity: `git -c user.email=dev.ungheni@gmail.com -c user.name=nalyk commit -m "..."`
- No `Co-Authored-By` lines. No "Generated with Claude Code" anywhere.
- Commit before declaring done — uncommitted work is negligence.

<critical_mandatory_rule>
Don't flatter me. Use radical candor when you communicate with me. Tell me something I need to know even if I don't want to hear it
</critical_mandatory_rule>