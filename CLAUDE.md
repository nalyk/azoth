# azoth

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
