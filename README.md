<p align="center">
  <img src="assets/logos/azoth-logo.svg" alt="azoth logo" width="120" height="120">
</p>

# azoth

<p align="center">
  <a href="https://github.com/nalyk/azoth/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/nalyk/azoth?include_prereleases&sort=semver&display_name=tag&label=release&color=blue"></a>
  <a href="https://github.com/nalyk/azoth/releases"><img alt="Total downloads" src="https://img.shields.io/github/downloads/nalyk/azoth/total?label=downloads&color=blue"></a>
  <a href="#license"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue"></a>
  <a href="https://github.com/nalyk/azoth/attestations"><img alt="SLSA v1.0 build provenance" src="https://img.shields.io/badge/SLSA-v1.0-blueviolet"></a>
  <a href="rust-toolchain.toml"><img alt="Rust 1.80+" src="https://img.shields.io/badge/rust-1.80%2B-orange?logo=rust"></a>
  <a href="https://github.com/nalyk/azoth/releases/latest"><img alt="Platform: linux x86_64" src="https://img.shields.io/badge/platform-linux--x86__64-lightgrey?logo=linux"></a>
</p>

Contract-centric, event-sourced, provider-agnostic coding-agent runtime.
Interactive TUI. Rust workspace. Linux only.

<p align="center">
  <a href="assets/start_screen.png"><img alt="azoth starting a fresh session — status line reads 'no contract yet', 0 turns, ctx 0%; the composer invites 'what are we building?' against the PAPER dark palette" src="assets/start_screen.png" width="100%"></a>
  <br>
  <sub><em>Fresh session — empty contract, 0 turns, the composer invites the first prompt.</em></sub>
</p>

<p align="center">
  <a href="assets/main_screen.png"><img alt="azoth mid-conversation — collapsible thoughts, rendered markdown headings with a Crate / Purpose GFM table, per-turn usage chip '15.4k↓ 1.3k↑ t+15s', and the whisper row 'ready · ^K for commands' beneath the composer" src="assets/main_screen.png" width="100%"></a>
  <br>
  <sub><em>Live turn — collapsible thoughts, rendered markdown (headings, lists, GFM tables), per-turn usage chip, whisper row.</em></sub>
</p>

Every release ships with fmt + clippy clean, the full workspace test suite
green on Linux x86_64, and SLSA v1.0 build provenance attached.

## Honest status

### What works end-to-end

- Drive a real coding agent from a terminal against Anthropic, OpenAI,
  OpenRouter, or a local Ollama endpoint.
- Contract-driven turns commit or abort; sessions persist as JSONL;
  `/resume` brings back a prior session with full forensic detail.
- Four-lane composite retrieval (FTS5 full-text + tree-sitter symbols +
  ripgrep + co-edit graph) with RRF fusion and per-lane token budget.
- Opt-in Linux sandbox (`AZOTH_SANDBOX=tier_a|tier_b`) puts `bash`
  inside user-ns + net-ns + Landlock, with optional fuse-overlayfs for
  Tier B stage-and-commit semantics.
- TDAD test-impact selection via `cargo test --list` (opt-in,
  `AZOTH_IMPACT_ENABLED=true`) surfaces only the tests a given diff
  actually exercises.
- `azoth eval run --live-retrieval <repo>` scores the retrieval plane
  against a seed corpus with localization@k.

### What is deliberately limited

- **Retrieval is keyword-grade for prose queries.** The composite
  works well when the prompt contains identifiers or paths. Natural-
  language "explain what happens when X" prompts lose signal — a
  query-planning / embedding lane is v2.5 scope.
- **`AZOTH_SANDBOX` defaults to off.** The jail imposes a ~100 ms
  overhead per tool call and needs unprivileged user namespaces
  (check with `unshare -U true`). Opt in when you want the enforcement.
- **Tree-sitter symbols: Rust only.** Python/TS/Go/Java grammars are
  v2.1 scope. Other languages still get FTS + ripgrep + co-edit graph.
- **TDAD: `cargo test` only.** pytest / jest / go test adapters are
  v2.1 scope.
- **Linux only.** Sandbox tiers and fuse-overlayfs are Linux-specific;
  macOS/Windows builds currently fail at the `sandbox` module. WSL2
  works.
- Contract amendments, the policy DSL, gVisor (Tier C), Firecracker
  (Tier D), domain packs, and enterprise deployment modes are all
  post-v2 scope per `docs/draft_plan.md`.

## Seven invariants

The runtime enforces these at all times. A code path that violates any
invariant is a bug regardless of compile-passing status.

1. **Transcript is not memory.** `ContextKernel` recompiles each
   request from durable state; no transcript replay.
2. **Deterministic controls outrank model output.** `AuthorityEngine`
   has final say on approvals, capability tokens, effect budgets.
3. **Every non-trivial run has a contract** with explicit success
   criteria and a side-effect budget.
4. **Every side effect has a class** — one of
   `observe | stage | apply_local | apply_repo | apply_remote_reversible
   | apply_remote_stateful | apply_irreversible`.
5. **Every run leaves structured evidence** — JSONL session, SQLite
   mirror, content-addressed artifacts.
6. **Every subsystem is eval-able.** Retrieval quality, validator
   outcomes, impact-selector decisions all emit measurable signals.
7. **Turn-scoped atomicity.** Every turn emits `turn_started` followed
   by exactly one of `turn_committed | turn_aborted | turn_interrupted`.

## Architecture

Three crates, strict one-way dependency arrow:

```
azoth (bin, TUI + CLI)
  └── azoth-repo (indexer plane: FTS5, symbols, co-edit graph, TDAD)
        └── azoth-core (runtime library, zero frontend deps)
```

`azoth-core` has ZERO heavy-indexer deps. Tree-sitter, rusqlite+FTS5,
git shell-out, and TDAD back-ends all live in `azoth-repo`.

### Turn pipeline

```
User input / Contract goal
        │
        ▼
TurnDriver.drive_turn()
  ├─ plan      — gather contract + last checkpoint
  ├─ compile   — ContextKernel builds a 5-lane packet
  │             (constitution · working_set · evidence · checkpoint · exit_criteria)
  ├─ invoke    — ProviderAdapter.invoke() streams into mpsc(64)
  ├─ dispatch  — ToolDispatcher extracts typed input, taint-gates,
  │             routes through SandboxPolicy, runs Tool::execute
  ├─ validate  — ContractGoal, Impact, project-local validators
  └─ commit    — JsonlWriter fsyncs, SqliteMirror indexes, artifacts
                 land by SHA256, turn_committed emitted
```

### Evidence lanes (composite retrieval)

Four lanes feed the `evidence` lane of each packet; a
`ReciprocalRankFusion` reranker merges them with a per-lane-floor
token budget:

| Lane      | Backend                                           | Query shape                      |
|-----------|---------------------------------------------------|----------------------------------|
| `graph`   | `CoEditGraphRetrieval` (git log)                  | co-edit neighbours of seed paths |
| `symbol`  | `SqliteSymbolIndex` (tree-sitter, Rust)           | exact / fuzzy identifier lookup  |
| `lexical` | `RipgrepLexicalRetrieval` (fixed strings)         | literal substring across repo    |
| `fts`     | `FtsLexicalRetrieval` (SQLite FTS5 porter)        | tokenised full-text              |

The graph lane's seed extractor strips `:line(:col)?` suffixes (compiler /
grep output convention) before querying. FTS snippets are normalised for
byte-stability across reindex (cache-prefix-stable).

## Requirements

- Rust 1.80+ stable
- Linux (tested on kernel 5.15+, WSL2 works)
- Optional: `fuse-overlayfs` on PATH for Tier-B sandbox
- Optional: `cargo` on PATH for TDAD impact selection

## Build

```bash
cargo build --release
```

## Quickstart

```bash
# Local Ollama (default profile, no API key needed)
cargo run

# Anthropic
AZOTH_PROFILE=anthropic ANTHROPIC_API_KEY=sk-ant-... cargo run

# OpenRouter with a specific model
AZOTH_PROFILE=openrouter \
  AZOTH_MODEL=openrouter/quasar-alpha \
  OPENROUTER_API_KEY=sk-or-v1-... \
  cargo run

# Resume a prior session
cargo run -- resume <run_id>

# Replay / export a prior session for forensics
cargo run -- replay <run_id> --format json
cargo run -- export <run_id> --format markdown --output transcript.md

# Eval sweep (seed-vs-seed baseline)
cargo run -- eval run --seed docs/eval/v2_seed_tasks.json --k 5

# Eval sweep against live retrieval
cargo run -- eval run --seed docs/eval/v2_seed_tasks.json --k 5 --live-retrieval .
```

## Provider profiles

| Profile                  | Adapter                | Default endpoint                     | Auth env                 |
|--------------------------|------------------------|--------------------------------------|--------------------------|
| `ollama-qwen-anthropic`  | AnthropicMessages      | `http://localhost:11434`             | (none)                   |
| `ollama-qwen-openai`     | OpenAiChatCompletions  | `http://localhost:11434/v1`          | (none)                   |
| `anthropic`              | AnthropicMessages      | `https://api.anthropic.com`          | `ANTHROPIC_API_KEY` (also accepts OAuth `sk-ant-oat01-*`) |
| `openai`                 | OpenAiChatCompletions  | `https://api.openai.com/v1`          | `OPENAI_API_KEY`         |
| `openrouter`             | OpenAiChatCompletions  | `https://openrouter.ai/api/v1`       | `OPENROUTER_API_KEY`     |

Per-session overrides: `AZOTH_BASE_URL`, `AZOTH_MODEL`, `AZOTH_API_KEY`.

## Sandbox

Opt in via `AZOTH_SANDBOX`:

| Value     | Mechanism                                                  |
|-----------|------------------------------------------------------------|
| (unset) / `off` | No sandbox. Tools run in the azoth process. (default.)           |
| `tier_a`  | Unprivileged user-ns + net-ns + Landlock V2 FS rules.       |
| `tier_b`  | Tier A + `fuse-overlayfs` merged mount of the repo; bash's cwd is the merged view. Successful writes stage back; failed runs discard. |

Graceful degradation chain: `tier_b` → (no fuse-overlayfs) → `tier_a` →
(no user-ns) → `off` → (jail setup fails at spawn) → `off`. Every step
logs a `tracing::warn` to `.azoth/azoth.log`.

Example bash invocation under Tier B:

```bash
AZOTH_SANDBOX=tier_b cargo run
# then in TUI:
#   /contract verify sandbox
#   bash: echo hello > hello.txt && rm old.txt
# stage_overlay_back copies hello.txt into the repo and propagates the
# rm as a whiteout. `out.staged_files` lists ["hello.txt"];
# `out.removed_files` lists ["old.txt"].
```

`BashOutput.staged_files` / `removed_files` are empty under `off` and
`tier_a` (writes go directly to the real repo).

## Retrieval configuration

| Env                       | Default      | Meaning                                                                 |
|---------------------------|--------------|-------------------------------------------------------------------------|
| `AZOTH_RETRIEVAL_MODE`    | `composite`  | `composite` fuses four lanes via RRF; `legacy` is single-lane lexical.  |
| `AZOTH_LEXICAL_BACKEND`   | `fts`        | `fts` (SQLite FTS5), `ripgrep`, `both` (composite uses both lanes), `ripgrep_fallback` when FTS unavailable. |
| `AZOTH_IMPACT_ENABLED`    | `false`      | `true` wires `CargoTestImpact` so validators run only tests impacted by the turn's diff. |

## Tools (all shipped)

| Tool              | Effect class   | Description                                                          |
|-------------------|----------------|----------------------------------------------------------------------|
| `repo_search`     | Observe        | Literal substring search via ripgrep (honours `.gitignore`).        |
| `repo_read_file`  | Observe        | Read a file by path with optional line range.                       |
| `repo_read_spans` | Observe        | Batch read multiple named line ranges.                              |
| `fs_write`        | ApplyLocal     | Write a file inside the repo root; approval required.               |
| `bash`            | ApplyLocal     | Run a shell command with timeout + cancellation; sandbox-aware.     |

Tool names are ASCII snake-case to satisfy the Anthropic Messages API
regex `^[a-zA-Z0-9_-]{1,128}$` (`ToolDispatcher::register` enforces).

## TUI controls

| Key                 | Action                                               |
|---------------------|------------------------------------------------------|
| Enter               | Send the current message                             |
| Alt+Enter           | Newline (multi-line input)                           |
| Up / Down           | Input history (when input is empty)                  |
| Mouse wheel         | Scroll transcript                                    |
| Shift+Up/Down       | Scroll one line                                      |
| Ctrl+Up/Down        | Scroll five lines                                    |
| PageUp / PageDown   | Scroll ten lines                                     |
| Ctrl+End            | Jump to bottom (auto-scroll)                         |
| y / s / n           | Grant once / grant session / deny (approval modal)   |
| Ctrl+C              | Cancel current turn / quit                           |

## Slash commands

| Command           | Description                                                                    |
|-------------------|--------------------------------------------------------------------------------|
| `/help`           | List commands                                                                  |
| `/status`         | Run ID, session path, turn count, active contract                              |
| `/context`        | Show the latest compiled context packet summary                                |
| `/contract <goal>`| Draft + accept a run contract                                                  |
| `/approve [tool]` | Pre-approve a tool for the session (empty arg lists active capability tokens)  |
| `/resume <run_id>`| Restart into a prior session                                                   |
| `/continue`       | Nudge the model to resume a turn aborted with `reason: "model_truncated"`      |
| `/quit`           | Exit                                                                           |

## CLI subcommands

```
azoth                              # launch TUI (default)
azoth tui                          # explicit TUI launch
azoth resume <run_id>              # resume a session
azoth replay <run_id> [--forensic] [--format text|json] [--sessions-dir <dir>]
azoth export <run_id> [--format markdown|json] [--output <path>] [--sessions-dir <dir>]
azoth version                      # print build info
azoth eval run --seed <path> [--k 5] [--out <path>] [--sessions-dir <dir>]
                                   [--run-id <id>] [--live-retrieval <repo>]
```

`replay` without `--forensic` emits only committed turns (safe for
resume context). `replay --forensic` includes aborted and interrupted
turns with `non_replayable: true` annotations.

## Session storage

```
.azoth/
  sessions/<run_id>.jsonl    # append-only turn-scoped event log (authoritative)
  state.sqlite               # indexed mirror (rebuildable from JSONL)
  artifacts/<sha256>         # content-addressed blob store
  azoth.log                  # tracing output (TUI writes here; stdout is ratatui)
```

JSONL is authoritative. SQLite is a secondary index rebuildable via
`event_store::rebuild_from(jsonl)`. Artifacts are content-addressed
(SHA256); tool outputs, packet evidence, and other large payloads are
stored out-of-band and referenced from events.

## Testing

```bash
cargo test --workspace -- --test-threads=1
# 466 passed / 0 failed / 1 ignored
```

Single-threaded because `sandbox_tier_a_smoke` and a few other tests
fork, which is fragile under parallel test harness runners. Clippy +
fmt must stay clean:

```bash
cargo fmt --check
cargo clippy --workspace --tests -- -D warnings
```

Selected integration tests worth knowing about:

- `sandbox_tier_a_smoke` — `spawn_jailed` + `/bin/true` round-trip
  (skips cleanly without unprivileged user-ns).
- `bash_tier_a_landlock_blocks_write_to_etc_passwd` — empirical proof
  Landlock denies out-of-sandbox writes.
- `bash_tier_b_stages_writes_back_to_repo_on_success` — overlay
  commit semantics.
- `bash_tier_b_blocks_symlink_dir_traversal_to_host_files` — security
  regression guard against `ln -s /etc leak`.
- `bash_tier_b_skips_fifo_entries_without_hanging` — `tokio::time::
  timeout` bounds a stage-back hang that `std::fs::copy` on a FIFO
  would introduce.
- `live_retrieval_against_real_tempdir_repo_produces_nonzero_predictions`
  — end-to-end FTS index build + composite query over a tempdir repo.

## Project structure

```
azoth/
├── Cargo.toml                    # workspace root
├── crates/
│   ├── azoth-core/               # runtime library, zero frontend + zero heavy-indexer deps
│   │   └── src/
│   │       ├── schemas/          # Contract, Turn, ContentBlock, EffectClass, Origin, etc.
│   │       ├── event_store/      # JSONL dual projection + SQLite mirror + hand-rolled migrator
│   │       ├── artifacts/        # SHA256 blob store
│   │       ├── contract/         # draft, lint, accept
│   │       ├── context/          # ContextKernel, CompositeEvidenceCollector, RRF, token budget
│   │       ├── retrieval/        # LexicalRetrieval, SymbolRetrieval, GraphRetrieval traits
│   │       ├── authority/        # Tainted<T>, SecretHandle, capability tokens, approvals
│   │       ├── sandbox/          # Tier A/B/C/D policy, spawn_jailed, OverlayWorkspace
│   │       ├── execution/        # Tool trait, ErasedTool, ToolDispatcher, ExecutionContext
│   │       ├── tools/            # repo_search, repo_read_file, repo_read_spans, fs_write, bash
│   │       ├── adapter/          # AnthropicMessages, OpenAiChatCompletions, SSE parsers
│   │       ├── turn/             # TurnDriver state machine (biased tokio::select!)
│   │       ├── validators/       # Validator + ImpactValidator + selector-backed variant
│   │       ├── eval/             # localization@k, regression_rate
│   │       └── telemetry/        # structured tracing emitters
│   ├── azoth-repo/               # heavy indexer plane (v2)
│   │   └── src/
│   │       ├── indexer.rs        # RepoIndexer — four-phase incremental reindex
│   │       ├── fts.rs            # FtsLexicalRetrieval — SQLite FTS5 porter unicode61
│   │       ├── code_graph/       # tree-sitter Rust symbol extraction + SqliteSymbolIndex
│   │       ├── history/          # git shell-out, co-edit graph builder, CoEditGraphRetrieval
│   │       └── impact/           # CargoTestImpact, GitStatusDiffSource
│   └── azoth/                    # binary crate (CLI + TUI)
│       └── src/
│           ├── main.rs           # clap, tracing setup
│           ├── eval.rs           # `eval run` subcommand
│           ├── eval_live.rs      # `--live-retrieval` composite builder
│           ├── replay.rs         # `replay` subcommand
│           ├── export.rs         # `export` subcommand
│           └── tui/              # ratatui + crossterm + tui-textarea
│               ├── app.rs        # AppState, biased select! loop, worker task
│               ├── config.rs     # profile registry, env overrides, OAuth
│               ├── render.rs     # frame builder, scrollbar, status line
│               └── widgets/      # approval modal, scrollback, input
├── docs/
│   ├── draft_plan.md             # v1 architecture spec (frozen anchor)
│   ├── v2_plan.md                # v2 plan (repo intelligence moat)
│   ├── eval/                     # seed tasks for localization@k
│   ├── superpowers/              # design docs per feature
│   └── research/                 # upstream research notes
└── .azoth/                       # runtime state (gitignored)
```

## Version history

- **v2.0.1** (current, main) — Tier-B `stage_overlay_back` symlink-escape
  hardening. Three rounds: refuse symlinks whose canonical target escapes
  the merged view; canonicalize against `ws.merged` (not `repo_root`) so
  same-turn-created targets validate independently of `read_dir` order;
  refuse every absolute symlink target outright (absolute paths captured
  from `$PWD` under the ephemeral overlay mount dangle forever post-unmount).
- **v2.0.0** — composite retrieval, tree-sitter symbols, co-edit graph,
  TDAD impact, eval plane, sandbox Tier-A/B enforcement on bash,
  live-retrieval flag, seven bot-review rounds closed.
- **v1.5** — adapters (Anthropic OAuth + OpenAI), content-block
  protocol, JSONL dual projection, Tier-A/B sandbox smoke, ContextKernel
  v0, Tools + ToolDispatcher + AuthorityEngine, TUI MVP.

See `docs/draft_plan.md` for the v1 architecture spec (still the
invariant anchor) and `docs/v2_plan.md` for the v2 plan (most items
shipped; the `§scope fence` is up-to-date).

## License

Dual-licensed under MIT OR Apache-2.0. See [LICENSE-MIT](LICENSE-MIT)
and [LICENSE-APACHE](LICENSE-APACHE).
