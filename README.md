# azoth

Contract-centric, event-sourced, provider-agnostic coding agent runtime. Interactive TUI. Rust.

## What it is

Azoth is a CLI agent runtime built for coding tasks. Every run has a contract with explicit success criteria, every side effect has a class, every turn leaves structured evidence. The runtime owns all continuity and caching — no reliance on provider-side stateful chaining.

Two crates:

- **azoth-core** — runtime library with zero frontend coupling. Schemas, event store, authority engine, adapters, context kernel, tools, sandbox, turn driver, validators, retrieval, telemetry.
- **azoth** — binary crate with a ratatui TUI. Clap CLI, profile-based provider config, interactive approval flow.

## Architecture

```
TUI input
  |
  v
TurnDriver (state machine: plan -> compile -> invoke -> dispatch -> validate -> commit/abort)
  |
  +-- ContextKernel (5-lane packet compiler: constitution, working_set, evidence, checkpoint, exit_criteria)
  +-- ProviderAdapter (AnthropicMessages | OpenAiChatCompletions, streaming SSE)
  +-- ToolDispatcher (taint gate, typed extraction, effect classification)
  +-- AuthorityEngine (capability tokens, approval policy, Tainted<T> wrapper)
  +-- Validators (deterministic turn-exit checks)
  +-- JsonlWriter (dual projection: replayable + forensic, SQLite mirror)
```

Seven invariants enforced at runtime:

1. Transcript is not memory — the Context Kernel compiles per-step packets from durable state.
2. Deterministic controls outrank model output.
3. Every non-trivial run has a contract.
4. Every side effect has a class (`observe` / `stage` / `apply_local` / `apply_repo` / `apply_remote_*` / `apply_irreversible`).
5. Every run leaves structured evidence.
6. Every subsystem is eval-able.
7. Turn-scoped atomicity with `turn_started` / `turn_committed` / `turn_aborted` / `turn_interrupted` markers.

## Build

```
cargo build --release
```

Requires Rust 1.80+ stable. Linux only (sandbox uses user namespaces, Landlock, seccomp).

## Run

```
# Default: Ollama on localhost:11434, Anthropic-compatible endpoint
cargo run

# Resume a prior session
cargo run -- resume <run_id>

# Select a different provider profile
AZOTH_PROFILE=anthropic ANTHROPIC_API_KEY=sk-ant-... cargo run
AZOTH_PROFILE=openai OPENAI_API_KEY=sk-... cargo run

# Override any profile field
AZOTH_BASE_URL=http://other:11434 AZOTH_MODEL=llama3 cargo run
```

## Provider profiles

| Profile | Adapter | Default endpoint | Auth env var |
|---------|---------|-----------------|-------------|
| `ollama-qwen-anthropic` | AnthropicMessages | `http://localhost:11434` | (none) |
| `ollama-qwen-openai` | OpenAiChatCompletions | `http://localhost:11434/v1` | (none) |
| `anthropic` | AnthropicMessages | `https://api.anthropic.com` | `ANTHROPIC_API_KEY` |
| `openai` | OpenAiChatCompletions | `https://api.openai.com/v1` | `OPENAI_API_KEY` |
| `openrouter` | OpenAiChatCompletions | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` |

Override with `AZOTH_PROFILE`, `AZOTH_BASE_URL`, `AZOTH_MODEL`, `AZOTH_API_KEY`.

## TUI controls

| Key | Action |
|-----|--------|
| Enter | Send message |
| Alt+Enter | Newline (multi-line input) |
| Up / Down | Input history (when input is empty) |
| Mouse wheel | Scroll transcript |
| Shift+Up/Down | Scroll 1 line |
| Ctrl+Up/Down | Scroll 5 lines |
| PageUp/PageDown | Scroll 10 lines |
| Ctrl+End | Jump to bottom |
| y / s / n | Grant once / grant session / deny (approval modal) |
| Ctrl+C | Quit |

## Slash commands

| Command | Description |
|---------|-------------|
| `/help` | Show command list |
| `/status` | Run ID, session path, turn count, contract |
| `/context` | Last compiled context packet |
| `/contract <goal>` | Draft and accept a run contract |
| `/approve <tool>` | Pre-approve a tool for the session |
| `/quit` | Exit |

## Tools

| Tool | Effect class | Description |
|------|-------------|-------------|
| `repo.search` | Observe | Literal substring search across repo |
| `repo.read_file` | Observe | Read file with optional line range |
| `repo.read_spans` | Observe | Batch read named line ranges |
| `fs.write` | ApplyLocal | Write file inside repo root |
| `bash` | ApplyLocal | Run shell command with timeout and cancellation |

## Project structure

```
azoth/
  Cargo.toml                          # workspace root
  crates/
    azoth-core/                       # runtime library
      src/
        schemas/                      # serde types: Contract, Turn, ContentBlock, etc.
        event_store/                  # JSONL dual projection + SQLite mirror
        artifacts/                    # SHA256 content-addressed blob store
        contract/                     # draft, lint, accept
        context/                      # 5-lane ContextKernel + evidence collector
        retrieval/                    # ripgrep-backed lexical search
        authority/                    # Tainted<T>, SecretHandle, capability tokens
        sandbox/                      # Tier A (ns+landlock+seccomp), Tier B (fuse-overlayfs)
        execution/                    # Tool trait, ToolDispatcher, taint gate
        tools/                        # repo.search, repo.read_file, repo.read_spans, fs.write, bash
        adapter/                      # AnthropicMessages, OpenAiChatCompletions, SSE parsers
        turn/                         # TurnDriver state machine
        validators/                   # deterministic turn-exit validators
        telemetry/                    # structured tracing events
    azoth/                            # binary crate
      src/
        main.rs                       # clap CLI, tracing setup
        tui/
          app.rs                      # AppState, biased select loop, worker task
          config.rs                   # profile registry, env-var resolution
          render.rs                   # ratatui frame builder, scrollbar
          input/                      # slash command parser
          widgets/                    # approval modal, scrollback, status line
  docs/
    draft_plan.md                     # architecture spec
    research/                         # design research notes
```

## Session storage

```
.azoth/
  sessions/<run_id>.jsonl             # append-only turn-scoped event log
  state.sqlite                        # indexed mirror (rebuildable from JSONL)
  artifacts/<sha256>                   # content-addressed blobs
  azoth.log                           # tracing output (TUI mode)
```

## Testing

```
cargo test --workspace
```

110 tests across unit and integration suites covering schemas, event store, adapters (fixture + live HTTP), contract, context kernel, authority, sandbox, tools, turn driver, validators, and TUI state.

## License

Dual-licensed under MIT and Apache 2.0. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
