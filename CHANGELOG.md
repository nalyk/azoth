# Changelog

All notable changes to azoth are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions follow
[SemVer](https://semver.org/).

## [2.1.0] — 2026-04-24

### Added

- **`SymbolKind` variants** — `Class`, `Method`, `Interface`, `TypeAlias`,
  `Decorator`, `Package`. The enum stays `#[serde(rename_all = "snake_case")]`
  and every addition is `#[serde(default)]`-safe, so pre-2.1 JSONL sessions
  and SQLite mirrors replay without loss. Pinned by
  `crates/azoth-core/tests/v2_1_forward_compat.rs`.
- **Language dispatcher** — `code_graph::detect_language` +
  `code_graph::extract_for` + `code_graph::parser_for` route extraction by
  file extension. New `.tsx` (tsx grammar factory) and path-aware parser
  selection live here. `Language::all_extractor_wired()` is the single
  source of truth for which grammars PRs B/C/D widen.
- **tree-sitter grammars for Python, TypeScript, and Go** —
  `tree-sitter-python = "0.21"`, `tree-sitter-typescript = "0.21"`
  (`.ts` via `language_typescript()`, `.tsx` via `language_tsx()`), and
  `tree-sitter-go = "0.21"`. Each language ships its own iterative
  `fn walk` (tree-sitter node kinds are grammar-specific, so the walker
  stays per-language) — all four share the TreeCursor-reuse pattern
  established in PR #20 round 5 to avoid stack overflow on deep
  fixtures. `code_graph/common.rs` holds the grammar-independent
  helpers (`short_digest`, `line_range`, `name_via_field`) reused
  across every walker. `.d.ts` / interface / `declare class` members
  extract via `function_signature` + `method_signature` classifier arms.
- **TDAD test-impact backends for pytest, jest, and `go test`** —
  `PytestImpact`, `JestImpact`, `GoTestImpact` (implement `ImpactSelector`)
  plus `PytestRunner`, `JestRunner`, `GoTestRunner` (implement a new shared
  `TestRunner` trait in `impact/runner.rs`). Each backend ships structured
  output from day one: `pytest -v` parser, `jest --json`, and `go test -json`
  NDJSON. `word_boundary_contains` helper in `impact/heuristic.rs` is reused
  across all four runners (cargo/pytest/jest/gotest). Jest monorepo and
  workspaces shapes refuse with a typed `JestError::UnsupportedConfig`. Go
  uses parent-directory-path matching (not file-stem — the test unit in Go
  is the package).
- **Red-team corpus +20 cases** — 20 new cases in `src/red_team.rs`
  across five categories (path-traversal, unicode-normalization, FTS5
  snippet prompt-escape, symbol shell-metacharacter, origin-spoofing).
  Exhaustive-origin check converted to `match` to enforce compile-time
  coverage of every `Origin` variant.
- **Eval seed expanded to 50 tasks** — `docs/eval/v2.1_seed_tasks.json`
  carries 30 new hand-labelled localization tasks (10 Python targeting
  `psf/requests`, 10 TypeScript targeting `microsoft/vscode-eslint`, 10
  Go targeting `urfave/cli`) alongside the original 20 v2 Rust tasks.
  `tests/eval_v2_1_seed.rs` gate-tests the 50-task count, per-language
  partition, unique task IDs, and seed-mode `localization@5 ≥ 0.45`.
- **Dogfood writeups** — `docs/dogfood/v2.1/{python,typescript,go}-session.md`
  each drive `azoth eval run --live-retrieval` against a cloned real
  repo (psf/requests @ 93bf533, microsoft/vscode-eslint @ 93b96ab,
  urfave/cli @ b79d768) and report symbol counts, short-identifier
  probe recall, and FTS5 phrase-literality notes.

### Changed

- **`AZOTH_SANDBOX` default flipped `off` → `tier_a`.** When the host
  supports unprivileged user namespaces, bash tools now run inside a
  user-ns + net-ns + Landlock jail by default. Opt out with
  `AZOTH_SANDBOX=off`. Hosts without user-ns support (old kernels,
  locked-down containers) degrade to `off` automatically with a
  one-shot `tracing::warn`. The userns probe is cached in a
  `OnceLock<bool>` and pre-warmed from `fn main()` before the Tokio
  multi-thread runtime starts, guarded by a thread-identity check
  (`OnceLock<ThreadId>`) so cold-cache forks from any multi-threaded
  caller (Tokio, `std::thread`, Rayon) fail closed rather than violate
  the `fork()` SAFETY precondition.

### Non-scope (explicit — deferred beyond 2.1)

- **JavaScript grammar** (`.js` / `.jsx` / `.mjs` / `.cjs`) — not in
  2.1. The TypeScript grammar handles `.ts` and `.tsx` only.
- **Jest workspaces / monorepo configs** — detected at selector entry
  and rejected with `JestError::UnsupportedConfig`.
- **LSP integration** — deferred for structural reasons (turn-atomicity
  conflict with per-server cross-turn state); see `docs/v2_plan.md`
  `§LSP` for the full rationale.
- **`gix` / `git2` structured git** — shell-out to `git` stays for 2.1.
  Revisit if the CLI boundary proves insufficient.
- **Go parallel per-package execution, `go.work` multi-module support,
  `-bench`-aware runner, partial-failure resilience across packages** —
  all land in 2.2 per the deferrals recorded on PR #26.

## [2.0.2] — 2026-04

Chronon Plane — invariant #8: *time is taint, not preface*. Every
persisted timestamp flows through an injected `Clock` (`SystemClock` in
production, `FrozenClock` in tests, `VirtualClock` for replay).
Externally-observed facts carry `(observed_at, valid_at)`. Contracts
may bound wall-clock spend via `scope.max_wall_secs`; open turns emit
throttled `TurnHeartbeat` events. `azoth resume --as-of <ISO8601>`
reconstructs a forensic projection at any wall-clock point (m0007 adds
the `turns.at` index).

## [2.0.1] — 2026-04

Tier-B `stage_overlay_back` symlink-escape hardening. Refuse symlinks
whose canonical target escapes the merged view; canonicalize against
`ws.merged` (not `repo_root`); refuse every absolute symlink target
outright.

## [2.0.0] — 2026-04

Composite retrieval (graph → symbol → lexical → FTS5 → rerank),
tree-sitter Rust symbols, co-edit graph from git history, TDAD impact
selector (cargo), eval plane (localization@k + regression rate),
sandbox Tier-A/B enforcement on bash, `--live-retrieval` flag.

## [1.5] — 2026-03

Adapters (Anthropic OAuth + OpenAI Chat Completions), Anthropic
content-block protocol internally, JSONL dual projection, Tier-A/B
sandbox smoke, `ContextKernel` v0, Tools + `ToolDispatcher` +
`AuthorityEngine`, TUI MVP.
