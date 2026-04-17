# azoth v2 — Repo Intelligence Moat

## Context

**Where we are.** v1.5 shipped (origin `master` @ b12d37f). The runtime is stable: 110+ tests, real adapters, contract-driven TurnDriver, JSONL dual projection, Tier A/B sandbox, replay + export. Research docs (`docs/research/research_01.md §17`, `docs/research/deep-research-report.md §v2`) prescribe v2 explicitly as the **"repo intelligence moat"** — the phase where azoth stops being "another CLI with tools" and starts getting structurally better on repo-scale coding work than a lexical agent can.

**Why it matters now.** Five of the seven invariants assume the Knowledge Plane is measurable (invariant 6). Today it isn't — retrieval is lexical-only, evidence is a pre-composed list with no lane telemetry, and the `GraphRetrieval` trait exists only as `NullGraphRetrieval`. Shipping v2 also closes invariant 6's teeth: every new subsystem in v2 emits eval signals from day one.

**Intended outcome.** A repo-aware azoth that localizes faster, retrieves more precisely, selects impact-affected tests, and measures its own retrieval quality — without compromising the seven invariants, the schema stability boundary, or the cache-prefix-stable 5-lane ordering.

**Brutal scope realism.** The research `§v2` list is nine deliverables, several of which explode on contact. This plan **cuts three items to v2.5** (LSP lifecycle, real reranker inference, full taint DSL enforcement) to keep v2 shippable in ~10 weeks. The cuts are load-bearing and defended below.

---

## Strategic cuts from research `§v2`

| Research v2 item | Decision | Reason |
|---|---|---|
| tree-sitter symbol extraction | **In v2** (Rust only) | Dogfood; single grammar; deterministic |
| LSP defs/refs/diagnostics | **→ v2.5** | Turn-atomicity risk (servers hold state across turns); multi-server lifecycle; degraded-mode policy not yet defined. Tree-sitter covers 80% of retrieval value |
| Co-edit graph from git history | **In v2** (shell out to `git`) | Needed for impact selection; shell-out defers `gix`/`git2` dep decision |
| SQLite FTS5 | **In v2** | Cheap, high-leverage; runs alongside ripgrep behind a feature flag for one sprint |
| TDAD test impact selection | **In v2** (Rust `cargo test` only) | Python/TS/Go ecosystems each a separate build; ship the dogfood case |
| Context Kernel v2 (graph→lex→fts→rerank) | **In v2** | The whole point of the moat — composite collector + statistical reranker (RRF) |
| Cross-encoder reranker | **→ v2.5** | Trait lands in v2; inference impl deferred. Ship `IdentityReranker` + `ReciprocalRankFusion` (statistical, no model) |
| Expanded taint (full DSL) | **Partial in v2** | Add `Origin::Indexer` variant only; enforcement DSL in v2.5 |
| Eval metrics | **In v2** (localization@k only) | "Mergeability proxy" needs a PR corpus that doesn't exist; demote to research task |

**The one-slide rationale.** If the engineer treats `§v2` as nine-of-nine, one quarter disappears on rust-analyzer lifecycle bugs and the other on per-ecosystem test discovery — and we never ship the thing that makes v2 valuable: symbol index + FTS5 in the retrieval order. Ship the boring wins. Earn the hard wins.

---

## Architecture

Three architectural calls made upfront, before any sprint starts:

### A1. New workspace crate `azoth-repo`

Isolates heavy deps (`tree-sitter`, `tree-sitter-rust`, SQLite FTS5 feature flag, git shell-out wrapper, future `async-lsp`) from `azoth-core`. Dependency arrow stays one-way:

```
azoth (bin) ──> azoth-core (lib) ──> azoth-repo (lib, optional)
                 │
                 └── schemas, traits, kernel, authority, dispatcher (unchanged)
```

`azoth-core` defines the retrieval traits and the `Origin` enum. `azoth-repo` implements them against real indexes. Downstream embedders (daemon mode, SDK) can depend on `azoth-core` alone and get `Null*` defaults. This preserves the "zero frontend coupling" rule spelled out in `.claude/rules/crate-boundary.md` and extends it to heavy indexing deps.

### A2. `ExecutionContext::builder()` (prep PR, before Sprint 0)

`crates/azoth-core/src/execution/context.rs` carries five fields today (`repo_root`, `artifacts`, `cancellation`, `run_id`, `turn_id`). v2 needs to thread three more handles (`retrieval`, `graph`, future `lsp`). Five tool files plus tests construct `ExecutionContext { .. }` by literal. Without a builder, **every v2 PR fights merge conflicts in unrelated tool tests**. One 50-line prep PR replaces every literal with `ExecutionContext::builder(..).build()` and is a pure refactor. This is work #0, before Sprint 0.

### A3. Hand-rolled migrator (no `refinery`)

`refinery` pulls ~30 transitive deps and adds an MSRV surface for a problem that is 120 lines of code. `crates/azoth-core/src/event_store/sqlite.rs:78-101` currently hard-fails on `PRAGMA user_version` mismatch — blocker for every v2 schema change. Replace with an ordered `Vec<fn(&Transaction) -> Result<()>>` dispatcher guarded by `user_version`; migrations live as `.rs` files with inline SQL, tested via a forward-compat integration test. This is PR-0 of v2 proper.

### A4. Additive extensions to existing types (backward-compat preserved)

- `Origin` enum gains one variant: `Indexer` (tree-sitter + FTS5 outputs). Serde `rename_all = "snake_case"` on the enum today means existing logs deserialize unchanged.
- `Edge { kind: String }` gains `#[serde(default = "one_f32")] weight: f32` — old logs replay as 1.0.
- `SessionEvent` gains six new variants, all with `#[serde(default)]` on optional fields. Never mutate existing variants.
- `EvidenceItem` gains two optional fields: `lane: Option<String>` and `rerank_score: Option<f32>`. Default None on v1.5 replay.
- `Validator` trait is **not modified**. A new trait `ImpactValidator` ships alongside for TDAD.
- `ContextKernel::compile()` signature is **not modified**. v2 changes are upstream of `compile()` — in a new `CompositeEvidenceCollector`.

---

## Sprint sequence

**PR-0: `ExecutionContext::builder()` refactor.** ~50 lines, pure refactor, no behavior change. Unblocks v2.

**Sprint 0 — Hand-rolled migrator (1 week).**
- Create `crates/azoth-core/src/event_store/migrations/mod.rs` — `pub fn run(conn: &mut Connection) -> Result<u32, MirrorError>`, ordered `Vec<MigrationStep>` inside `BEGIN IMMEDIATE ... COMMIT`, returns new `user_version`.
- Create `migrations/m0001_initial.rs` — idempotent; detect existing v1 `turns` table via `SELECT name FROM sqlite_master` before `CREATE`. Converges fresh DBs and v1.5 DBs to version 1.
- Replace `ensure_schema` at `event_store/sqlite.rs:78-101` with `migrations::run(&mut conn)`.
- Add `tests/migration_forward_compat.rs` — writes v1 sample DB, opens with v2 binary, asserts `user_version = 2`, assert rows preserved, assert new tables exist.
- **Verification:** `cargo test -p azoth-core migration_forward_compat` + existing event_store tests pass green.
- **Gate:** no later sprint merges until this is in.

**Sprint 1 — FTS5 + `FtsLexicalRetrieval` (1 week).**
- Upgrade `rusqlite` features in root `Cargo.toml:48` from `["bundled"]` → `["bundled", "fts5"]`. Verify sandbox smoke — seccomp filter at `crates/azoth-core/src/sandbox/` may need widening for new syscalls.
- Create `azoth-repo` crate (`crates/azoth-repo/Cargo.toml`, `src/lib.rs`). Wire into workspace.
- Migration `m0002_fts_schema.rs` — creates `documents(path PK, mtime, language, content)` + `documents_fts USING fts5(path, content, tokenize='porter unicode61')` with content-triggered sync.
- `azoth-repo/src/indexer.rs` — `RepoIndexer { conn, root }` with `async fn reindex_incremental()`; reuses the existing `ignore::WalkBuilder` so scope matches ripgrep. Mtime-gated upsert.
- `azoth-repo/src/fts.rs` — `FtsLexicalRetrieval` impl of `LexicalRetrieval`. Query via `documents_fts MATCH ?1 ORDER BY rank LIMIT ?2`, `snippet()` for inline context.
- Config knob `AzothConfig.retrieval.lexical_backend ∈ {ripgrep, fts, both}`; default `ripgrep` in Sprint 1, flip to `both` in Sprint 5 for eval, `fts` in v2.1.
- New `SessionEvent::RetrievalQueried { turn_id, backend, query, result_count, latency_ms }` — all optional fields `#[serde(default)]`.
- **Verification:** `tests/retrieval_parity.rs` asserts FTS returns ≥ ripgrep results for 20 seeded identifier queries. Sandbox smoke inside landlock passes.

**Sprint 2 — Tree-sitter symbol extraction (2 weeks, Rust only).**
- Workspace deps in `azoth-repo/Cargo.toml`: `tree-sitter = "0.22"`, `tree-sitter-rust = "0.21"`.
- `azoth-repo/src/code_graph/mod.rs` — `Symbol { id, name, kind, path, start_line, end_line, parent, language }`. `SymbolKind = Function | Struct | Enum | Trait | Impl | Module | Const | Mod | Fn`.
- `azoth-repo/src/code_graph/rust.rs` — `extract_symbols(path, src) -> Vec<Symbol>` via TS queries in `azoth-repo/queries/rust.scm`.
- `azoth-repo/src/code_graph/index.rs` — `SqliteSymbolIndex` implements a new `SymbolRetrieval` trait (lives in `azoth-core/src/retrieval/symbol.rs`):
  ```rust
  #[async_trait] pub trait SymbolRetrieval: Send + Sync {
      async fn by_name(&self, name: &str, limit: usize) -> Result<Vec<Symbol>, RetrievalError>;
      async fn enclosing(&self, path: &str, line: u32) -> Result<Option<Symbol>, RetrievalError>;
  }
  ```
  Default `NullSymbolRetrieval`. Indexing piggybacks on `RepoIndexer::reindex_incremental`.
- Migration `m0003_symbols.rs` — `symbols(id PK, name, kind, path, start_line, end_line, parent_id NULL, language, digest)` + `symbols_by_name_idx`. `digest` is file content hash at index time → invalidation.
- Wire into evidence: `SymbolEvidenceCollector { retrieval: Arc<dyn SymbolRetrieval> }` implements `EvidenceCollector`. Maps `Symbol` → `EvidenceItem { label: format!("lane:symbol {name}"), artifact_ref: Some(format!("{path}#L{start_line}")), inline: None, decision_weight: computed, lane: Some("symbol".into()), rerank_score: None }`.
- New `SessionEvent::SymbolResolved { turn_id, query, matched: Vec<SymbolId>, backend }`.
- **Verification:** `tests/symbol_extraction.rs` seeds 10 Rust files with known signatures; asserts extracted counts and `enclosing(..)` correctness. `tests/symbol_incremental.rs` asserts mtime-gated reindex doesn't re-parse unchanged files.

**Sprint 3 — Co-edit graph via git shell-out (1.5 weeks).**
- Dependency strategy: `Command::new("git")` via a thin wrapper in `azoth-repo/src/history/git_cli.rs`. Parse `git log --name-only --format='%H%n%ct'` over last 500 commits (configurable). No new dep; deferrable upgrade to `gix` later if needed. Trade-off accepted: typed-error loss for portability + sandbox-cleanliness win.
- `azoth-repo/src/history/co_edit.rs` — `build(repo, window) -> CoEditGraph`. For each commit, accumulate pair weights `w(a,b) += 1 / max(1, |files_in_commit| - 1)` over unordered file pairs. Squash-merge degeneracy noted in risk ledger.
- Migration `m0004_co_edit.rs` — `co_edit_edges(path_a, path_b, weight, last_commit_sha, PRIMARY KEY (path_a, path_b))` + `CHECK (path_a < path_b)` dedupe.
- `azoth-repo/src/history/graph_retrieval.rs` — `CoEditGraphRetrieval` implements the existing `GraphRetrieval` trait. `NodeRef("path:src/foo.rs")` → neighbors with `Edge { kind: "co_edit".into(), weight: computed }`.
- Extend `Edge` with `#[serde(default = "one_f32")] weight: f32` (additive).
- **Verification:** `tests/co_edit_graph.rs` builds a 50-commit synthetic repo via `git init` in a tempdir, asserts top-5 neighbors for a known file. Budget test: cold build on 500 commits ≤ 3s.

**Sprint 4 — Context Kernel v2 (composite + reranker) (2 weeks).**
- `azoth-core/src/context/reranker.rs` — `Reranker` trait:
  ```rust
  #[async_trait] pub trait Reranker: Send + Sync {
      async fn score(&self, query: &str, items: &[EvidenceItem]) -> Result<Vec<f32>, RerankError>;
  }
  ```
  Two impls in v2: `IdentityReranker` (pass-through), `ReciprocalRankFusion { k: 60.0 }` (statistical RRF across lane sources). Cross-encoder `BgeReranker` trait-registered but `unimplemented!()` in v2 — ships in v2.5.
- `azoth-core/src/context/budget.rs` — `TokenBudget { max_tokens, per_lane_floor: HashMap<&'static str, u32> }`. Per-lane floor prevents starvation.
- `azoth-core/src/context/composite.rs` — `CompositeEvidenceCollector` composes in order `graph → symbol → lexical → fts → rerank`. Each stage tags `EvidenceItem.lane`. Final list is rerank-scored, budget-truncated, weight-sorted.
- Wire into `TurnDriver`: swap `evidence_collector` field default from `LexicalEvidenceCollector` to `CompositeEvidenceCollector` behind `AzothConfig.retrieval.mode = composite | legacy`. Default legacy through end of Sprint 4, flip in Sprint 7.
- Kernel signature **unchanged**. `ContextKernel::compile()` still sorts by `decision_weight`.
- Critical: normalize FTS5 `snippet()` whitespace before lane insertion — cache-prefix-stable ordering depends on byte-stability.
- **Verification:** `tests/context_kernel_v2.rs` asserts lane ordering stable under reranker permutation. `tests/evidence_replay.rs` confirms v1.5 JSONL replays clean under v2 binary. `tests/context_budget_fairness.rs` asserts no lane starves under pathological weight distributions.

**Sprint 5 — TDAD test impact selection (Rust only) (2 weeks).**
- **Not a `Validator` extension** — sibling subsystem:
  ```rust
  #[async_trait] pub trait ImpactSelector: Send + Sync {
      async fn select(&self, diff: &Diff, contract: &Contract) -> Result<TestPlan, ImpactError>;
  }
  pub struct TestPlan { pub tests: Vec<TestId>, pub rationale: Vec<String> }
  ```
- `azoth-repo/src/impact/cargo.rs` — `CargoTestImpact` implements `ImpactSelector`. Discovers tests via `cargo test --list --format json`. Edge heuristic: `src/foo.rs` → `tests/*foo*` + `src/foo.rs` → symbols-in-foo → callers (via Sprint 2 symbol graph) → tests touching those callers + co-edit graph (Sprint 3) → adjacent tests.
- `azoth-core/src/validators/impact.rs` — `ImpactValidator { selector: Arc<dyn ImpactSelector>, runner: Arc<dyn TestRunner> }` — wraps `ImpactSelector` and implements the **new** `ImpactValidator` trait (not `Validator`). TurnDriver gains an `impact_validators` slot next to `validators`.
- Migration `m0005_impact.rs` — `test_impact(turn_id, test_path, status, confidence, selected_because, ran_at)`.
- New `SessionEvent::ImpactComputed { turn_id, changed_files: Vec<String>, selected_tests: Vec<String>, selector_version: u32 }`.
- **Verification:** `tests/tdad_impact.rs` — two-file diff asserts test plan includes direct test + co-edit-adjacent test. Integration run on azoth itself proves dogfood value.

**Sprint 6 — Eval plane (localization@k) (1 week).**
- `azoth-core/src/eval/mod.rs` — `EvalReport { localization_precision_at_k, regression_rate, sampled_at }`.
- `azoth-core/src/eval/localization.rs` — computes precision@k by comparing evidence lane membership against files modified in the turn's committed effects (uses `EffectRecord` events already persisted).
- `azoth-core/src/eval/regression.rs` — delta on ImpactValidator results vs. prior turn.
- Migration `m0006_eval.rs` — `eval_runs(run_id, turn_id, metric, value, sampled_at)`.
- New `SessionEvent::EvalSampled { turn_id, metric, value: f64 }`.
- Seed dataset: 20 hand-labeled azoth-repo tasks (file sets known). Lives in `docs/eval/v2_seed_tasks.json`. Manual effort; ~1 day.
- "Mergeability proxy" **not shipped** — demoted to research followup with named owner post-v2.
- **Verification:** `tests/eval_localization.rs` — seeds a known run, asserts precision@5 computation. `azoth eval run --seed v2_seed` CLI subcommand runs the 20 tasks headless against `MockAdapter` + `MockScript`.

**Sprint 7 — Integration + flip defaults (1 week).**
- Flip `retrieval.lexical_backend` default → `fts`.
- Flip `retrieval.mode` default → `composite`.
- `Origin::Indexer` variant added to taint enum; `dispatcher.rs:86` gate now knows it. FTS5/symbol evidence carries `Origin::Indexer`, not `ModelOutput`.
- Small red-team subset: 6–10 injection cases in `tests/v2_injection_surface.rs` (symbol names with `$(rm -rf)` payloads, FTS snippets with prompt-escape attempts). Prevents regression before v2.5 full red-team.
- Update `docs/draft_plan.md` — delete v2 from the "What v1 does NOT ship" scope fence.
- Update `CLAUDE.md` — note `azoth-repo` as third crate; update architecture constraints.
- Bump `crates/azoth-core/Cargo.toml` + `crates/azoth/Cargo.toml` + `crates/azoth-repo/Cargo.toml` to version `2.0.0`.
- **Verification:** full `cargo test --workspace`, headless dogfood run on azoth repo, manual TUI smoke. Release notes in commit message list the 9 new `SessionEvent` variants, the new `Origin::Indexer`, the new `Edge.weight` field.

---

## Critical files

**Modify:**
- `Cargo.toml` — add `rusqlite` `fts5` feature, add `azoth-repo` to workspace members
- `crates/azoth-core/src/event_store/sqlite.rs:78-101` — replace hard-fail with migrator dispatch
- `crates/azoth-core/src/execution/context.rs:58-73` — add builder (prep PR)
- `crates/azoth-core/src/authority/tainted.rs:10-17` — add `Origin::Indexer`
- `crates/azoth-core/src/retrieval/mod.rs` — extend `Edge` with optional `weight`
- `crates/azoth-core/src/schemas/event.rs` — add 6 new `SessionEvent` variants
- `crates/azoth-core/src/schemas/turn.rs` — extend `EvidenceItem` with optional `lane`, `rerank_score`
- `crates/azoth-core/src/turn/mod.rs:201` — wire `CompositeEvidenceCollector`
- `crates/azoth-core/src/telemetry/mod.rs` — add 5 new emit functions

**Create:**
- `crates/azoth-repo/` entire crate
- `crates/azoth-core/src/event_store/migrations/` (6 migration files)
- `crates/azoth-core/src/retrieval/symbol.rs` — `SymbolRetrieval` trait
- `crates/azoth-core/src/context/composite.rs` — composite collector
- `crates/azoth-core/src/context/reranker.rs` — reranker trait + RRF + Identity
- `crates/azoth-core/src/context/budget.rs` — per-lane token budgeting
- `crates/azoth-core/src/validators/impact.rs` — `ImpactValidator` trait + impl wrapper
- `crates/azoth-core/src/eval/mod.rs`, `localization.rs`, `regression.rs`
- 8+ new integration tests in `crates/azoth-core/tests/`

**Reuse (important):**
- `ignore::WalkBuilder` from existing `Cargo.toml:60` — shared scope semantics between ripgrep and FTS indexer
- `LexicalRetrieval` trait (unchanged) — `FtsLexicalRetrieval` is a drop-in sibling to `RipgrepLexicalRetrieval`
- `GraphRetrieval` trait (unchanged) — `CoEditGraphRetrieval` replaces `NullGraphRetrieval`
- `EvidenceCollector` pattern — `SymbolEvidenceCollector`, `FtsEvidenceCollector`, `CompositeEvidenceCollector` all follow existing `LexicalEvidenceCollector` shape
- `MockAdapter` + `MockScript` for headless tests
- `string_id!` macro for `SymbolId`, `RetrievalQueryId`

---

## Risk ledger (severity-ranked)

1. **Cache-prefix-stability drift from FTS5 snippet non-determinism.** `ContextKernel::compile()` hashes the evidence lane for cache keys. If FTS `snippet()` output varies across reindexes (whitespace, highlight markers), Anthropic prompt-cache hit rate collapses. **Mitigation:** Sprint 1 normalizer strips highlight markers and collapses whitespace before snippet enters `EvidenceItem.inline`. Test: `tests/evidence_snippet_stable.rs` reindexes twice and asserts byte-identical snippets.

2. **SQLite `user_version` migration breaks v1.5 sessions if Sprint 0 ships incomplete.** Combined PR = combined rollback. **Mitigation:** Sprint 0 ships as its own PR, with `migration_forward_compat.rs` as gate. `m0001_initial.rs` detects existing `turns` table before `CREATE`. Do not merge Sprint 1+ until Sprint 0 is in main.

3. **Co-edit graph degeneracy on squash-merge repos.** Every PR with N files becomes N choose 2 edges of weight 1 — dense, signal-free. **Mitigation:** document this limitation in `azoth-repo/src/history/co_edit.rs`; add config knob `co_edit.skip_large_commits: u32 = 50` to exclude outlier commits. Document that tests against azoth's own repo (rebase-merge) are the golden case; monorepo users may need custom windowing.

4. **Token budget starvation under pathological weights.** Composite collector runs greedy-by-weight post-rerank; one lane can crowd out others. **Mitigation:** `TokenBudget.per_lane_floor` guarantees minimum tokens per lane (defaults: graph=200, symbol=400, lexical=400, fts=400, checkpoint=200). `tests/context_budget_fairness.rs` asserts no lane hits zero under skewed weights.

5. **`ExecutionContext` mutation breaks tool tests at merge time.** Five tool files + tests construct `ExecutionContext { .. }` by literal. **Mitigation:** PR-0 (prep refactor) introduces `::builder()` before any v2 PR. Hard rule: reject any v2 PR that constructs `ExecutionContext` by literal.

6. **Tree-sitter grammar drift.** `tree-sitter` 0.22 vs. `tree-sitter-rust` 0.21 compat is real but fragile; CI must pin exact versions. **Mitigation:** `Cargo.lock` committed (workspace already commits it), `[patch.crates-io]` escape hatch documented.

7. **FTS5 cold index on large repos.** First session on a 500k-LOC repo takes minutes — user perceives hang. **Mitigation:** emit `tracing::info!` progress every 1000 files; add `azoth index --prewarm` subcommand for explicit indexing; degraded-mode fallback to ripgrep if index < 10% built.

---

## Scope decisions (surfaced, not deferred)

These are decisions I've made with reasoning. If any is wrong, flag before Sprint 0 starts — they compound.

1. **LSP → v2.5** (not v2). Reasons: protocol lifecycle, per-language server binaries as runtime dep, turn-atomicity violation when servers hold cross-turn state. Tree-sitter covers ~80% of retrieval value with none of the state-management pain.
2. **Tree-sitter: Rust only** in v2. Python/TS/Go ship in v2.1 as one-grammar-per-PR. Dogfooding on azoth itself proves the pattern.
3. **Git: shell out to `git`** (not `gix`, not `git2`). Defers the dep decision, no OpenSSL pain, sandbox-clean. Typed-error loss is acceptable for v2.
4. **New `azoth-repo` crate** (not extending `azoth-core`). Keeps `azoth-core` dep graph thin for downstream embedders; isolates tree-sitter/FTS5/git heavy deps. Requires one-line addition to `CLAUDE.md` and `.claude/rules/crate-boundary.md`.
5. **Hand-rolled migrator** (no `refinery`). 120 lines vs. 30 transitive deps. Migrations as `.rs` files, not `.sql` — lets us use `rusqlite::Transaction` typed API.
6. **Reranker: statistical only** (`IdentityReranker` + `ReciprocalRankFusion`). Cross-encoder inference → v2.5. Trait ships in v2 so no kernel re-surgery later.
7. **TDAD: cargo test only**. Pytest/jest/go test ecosystems → v2.1. Rust-on-Rust dogfood proves the ImpactSelector pattern.
8. **Eval: localization@k only**. Drop "mergeability proxy" as a shipping metric — no PR corpus exists. Keep as post-v2 research task.
9. **Taint: `Origin::Indexer` in v2**; full DSL enforcement in v2.5 with policy engine.

---

## Verification (end-to-end)

**Per-sprint:** each sprint's listed integration tests must pass; `cargo test --workspace -- --test-threads=1` green; `cargo clippy --workspace -- -D warnings` clean; `cargo fmt --check` clean.

**v2 ship gate (after Sprint 7):**

1. `cargo test --workspace` — all 110+ existing tests plus ~15 new v2 tests green.
2. `cargo test --workspace migration_forward_compat` — v1.5 DB opens clean under v2 binary, rows preserved, new tables present.
3. `cargo test --workspace evidence_replay` — v1.5 JSONL replays clean under v2 binary; unknown events gracefully skipped.
4. `cargo test --workspace context_budget_fairness` — no lane starves under skewed weights.
5. `cargo test --workspace evidence_snippet_stable` — FTS snippets byte-stable across reindex.
6. `cargo test --workspace v2_injection_surface` — all 6–10 red-team cases blocked.
7. **Dogfood run:** `AZOTH_PROFILE=anthropic cargo run -- ` on azoth itself with a known task ("find all tool impls"). Evidence lane should contain symbol hits (tree-sitter), FTS hits (docstrings), and co-edit neighbors. Emit events inspected manually for sanity.
8. **Eval headless:** `cargo run -- eval run --seed v2_seed` against 20 seed tasks via `MockAdapter`. Localization@5 ≥ 0.75 is the ship threshold (if lower, Sprint 4 reranker needs tuning before release).
9. **Manual TUI smoke:** launch TUI, type "grep for TurnDriver", observe evidence rendering with lane tags visible in status line. Use `/resume` on a v1.5 session file — no panic.
10. **Sandbox smoke:** tier-B sandbox with FTS5 feature enabled still denies `/etc/passwd` writes. Seccomp filter at `crates/azoth-core/src/sandbox/` still covers new rusqlite syscalls.

**Commit discipline:**
- Each sprint = one PR minimum, one logical unit.
- Commits use `git -c user.email=dev.ungheni@gmail.com -c user.name=nalyk` per CLAUDE.md.
- No `Co-Authored-By`. No "Generated with Claude Code".
- Release via `gh pr create --base main --head master` per memory (main is canonical).

---

## Out of scope (v2.5 and beyond)

Deferred to v2.5: LSP integration (full), policy DSL, full taint enforcement DSL, cross-encoder reranker inference, gVisor/Firecracker sandbox tiers, red-team harness (full), mergeability proxy eval.

Deferred to v2.1: Tree-sitter Python/TypeScript/Go grammars, pytest/jest/go test ImpactSelectors, `gix`/`git2-rs` structured git if shell-out proves insufficient.

Deferred to v3: domain packs (non-coding), enterprise deployment modes, provider routing/fallback, episodic memory beyond checkpoints.

This plan is the v2 skeleton. v2.5 and v3 plans reference this one as the prior invariant anchor.
