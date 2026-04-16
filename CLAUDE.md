# azoth

Contract-centric, event-sourced, provider-agnostic coding agent runtime.
Rust workspace: `azoth-core` (library, zero TUI coupling) + `azoth` (CLI/TUI binary). Linux only.

Full architecture spec: @docs/draft_plan.md

## Commands

```bash
source "$HOME/.cargo/env"          # required — cargo is NOT on default PATH
cargo check --workspace
cargo build --workspace
cargo test --workspace             # 110+ tests, unit + integration
cargo test -p azoth-core           # core crate only
cargo clippy --workspace -- -D warnings
cargo fmt --check
AZOTH_PROFILE=anthropic cargo run  # or: ollama-qwen-anthropic (default), openai, openrouter
```

## Seven Invariants

These are runtime laws. Code that violates any invariant is a bug regardless of whether it compiles.

1. **Transcript is not memory** — ContextKernel recompiles from durable state every turn
2. **Deterministic controls outrank model output** — AuthorityEngine has final say
3. **Every non-trivial run has a contract** — explicit goals + success criteria
4. **Every side effect has a class** — EffectClass enum (Observe, Stage, ApplyLocal, ApplyRepo, ApplyRemote*, ApplyIrreversible)
5. **Every run leaves structured evidence** — SessionEvents, checkpoints, artifacts
6. **Every subsystem is eval-able** — telemetry emits measurable signals
7. **Turn-scoped atomicity** — TurnStarted must be followed by exactly one of TurnCommitted / TurnAborted / TurnInterrupted

## Architecture Constraints

- `azoth-core` has ZERO frontend deps. Never import ratatui, crossterm, clap, or tui-textarea in azoth-core.
- Internal model protocol uses **Anthropic Messages content-block shape**. The OpenAI adapter downcasts on the wire. Do not introduce a third internal format.
- `schemas/` is the type hub — changes ripple everywhere. Treat it as a stability boundary.
- Every Tool impl must: (a) define a typed `Input` struct, (b) declare an `EffectClass`, (c) go through the taint gate via `ErasedTool` blanket. Never bypass the dispatcher.
- ContextKernel 5-lane ordering is **cache-prefix-stable**: constitution → working_set → evidence → checkpoint → exit_criteria. Never reorder.
- **JSONL is authoritative** (CRIT-1). SQLite mirror is a rebuildable secondary index. Never write to SQLite as primary store.
- Event lifecycle: TurnStarted → exactly one terminal marker. No orphaned events, no silent returns.
- Tiers C and D (`apply_remote_*`, `apply_irreversible`) return `EffectNotAvailable` in v1. Do not implement real dispatchers.
- Sandbox is Linux-only: Tier A (user namespaces + Landlock + seccomp), Tier B (+ fuse-overlayfs).

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

- Execute autonomously end-to-end. Do not present option menus or ask "would you like me to..."
- Run `cargo fmt`, `cargo clippy`, `cargo test --workspace` before declaring any task complete.
- Git identity: `git -c user.email=dev.ungheni@gmail.com -c user.name=nalyk commit -m "..."`
- No `Co-Authored-By` lines. No "Generated with Claude Code" anywhere.
- Commit before declaring done — uncommitted work is negligence.
