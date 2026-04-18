# v2 Closure — design

Status: approved for execution (user directive 2026-04-18).
Scope: close the three v2-plan acceptance gaps left open by v2.0.0.

## The three gaps (brutally stated)

1. **GRAPH LANE UNWIRED.** `CompositeEvidenceCollector.graph` slot is
   explicitly `None` in `crates/azoth/src/tui/app.rs:928`. The
   `CoEditGraphRetrieval` is built on worker startup
   (`crates/azoth/src/tui/app.rs:654`) but nothing consumes it from the
   composite. Plan's prescribed 4-lane fusion is actually 3 lanes in
   production.
2. **SANDBOX NOT ENFORCED.** `crates/azoth-core/src/execution/dispatcher.rs:87`
   admits in a comment: "spawn_jailed for subprocess isolation is v2.1+
   scope." `BashTool::execute` (`crates/azoth-core/src/tools/bash.rs:67`)
   runs `tokio::process::Command` directly against the host — no
   user-ns, no landlock, no seccomp. The jail machinery
   (`sandbox::tier_a::spawn_jailed`, `sandbox::tier_b::OverlayWorkspace`)
   exists and passes its integration smoke, but nothing in the runtime
   dispatch path calls it.
3. **EVAL SCORES ITSELF.** `crates/azoth/src/eval.rs:6-8` admits:
   "consumes `predicted_files` directly from the seed JSON" — the
   localization@k metric measures seed-authoring quality, not
   retrieval quality. Plan gate of ≥ 0.75 cannot even be honestly
   measured until live retrieval replaces `predicted_files`.

## Sequencing (low→high blast radius, three PRs)

**PR A — live retrieval eval.** Additive CLI flag
`--live-retrieval <repo>`. New module `azoth/src/eval_live.rs`. Builds
a composite collector against `<repo>`, runs `collect(prompt, k)` per
task, extracts path prefixes from `EvidenceItem.label`, overrides
`SeedTask.predicted_files` before `score_tasks`. Emits a new
`SessionEvent::EvalSampled { metric: "localization_precision_at_k_live", … }`.
**Blast radius: zero on existing code paths.** Default path
(no flag) still does seed-vs-seed.

**PR B — graph lane wiring.** New
`GraphEvidenceCollector { retrieval: Arc<dyn GraphRetrieval>, seed_paths_from_query: PathExtractor }`
in `azoth-core/src/context/graph_evidence.rs`. Query-path extraction
heuristic: lift `*.rs` / `*/path` fragments out of the query string;
for each, call `retrieval.neighbors(NodeRef("path:{p}"), depth=1, limit)`,
map edge targets to `EvidenceItem { label: format!("{target}"), lane: Some("graph"), decision_weight: (edge.weight * 100.0).round() as u32 }`.
Wire into `crates/azoth/src/tui/app.rs:928` when
`CoEditGraphRetrieval` is available. Plan's stated order
(graph→symbol→lexical→fts→rerank) is already coded in
`composite.rs:107-112`; wiring is the last missing step. **Blast
radius: low-medium.** Composite-collector tests all assume optional
lanes; the rerank + budget paths don't change shape.

**PR C — sandbox Tier-B wiring.** Add `spawn_jailed_tokio` in
`azoth-core/src/sandbox/tier_a.rs` that uses
`tokio::process::Command::pre_exec` to install the exact same
user-ns + net-ns + landlock + seccomp sequence as `spawn_jailed`, but
returns a `tokio::process::Child` with stdio pipes — so bash's
stdout/stderr capture keeps working. Extend `ExecutionContext` with
an optional `sandbox: Option<SandboxPolicy>` that toggles via
`AZOTH_SANDBOX=tier_a|tier_b` env var (default `off` — legacy tests
stay green). `BashTool::execute` branches on that policy: off →
current code; tier_a → `spawn_jailed_tokio` with landlock read-all,
write-only-to `/tmp`; tier_b → mount `OverlayWorkspace` at
repo_root, `current_dir=merged`, landlock write-only-to
`merged+/tmp`. New integration test
`tests/sandbox_bash_tier_b_smoke.rs` asserts EACCES on
`/etc/passwd` write and a legal write into `merged`. **Blast radius:
medium-high** — anything that invokes bash is touched. Mitigation:
default-off behind env var; all 349 existing tests stay green
without opting in.

## Architectural rulings (settled, not negotiable)

- **Live-retrieval output overrides `predicted_files` in-memory, not
  on disk.** The seed JSON stays the canonical source of truth for
  CI reproducibility; live retrieval is a sweep mode, not a seed
  rewrite. (See memory `arch_eval_seed_vs_live_retrieval`.)
- **Graph lane ships with a path-extractor that's deliberately
  dumb.** Greedy regex over the query for `[a-zA-Z0-9_/.-]+\.rs`
  gets us to signal; smarter extraction is v2.5 with the policy
  DSL. (See plan's "scope decisions" #1.)
- **Sandbox wiring defaults OFF.** `AZOTH_SANDBOX` opt-in preserves
  the 349-green-tests invariant. Flipping the default is a separate
  decision for v2.1 after we've measured overhead on the dogfood
  loop. (See memory `pattern_default_flip_is_no_op_without_consumer_wiring`
  — flipping a default without a consumer audit ships a lie.)
- **One PR per gap.** Sprint-gate discipline (memory
  `project_v2_sprint_gate_discipline`) — three atomic, rollback-able
  units.
- **Each PR is TDD.** Red test first, then implementation. Verified
  against `cargo test --workspace` green before each commit. (See
  memory `feedback_commit_before_done` + `feedback_honest_readiness_reporting`.)

## Verification gates after all three PRs

1. `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`,
   `cargo test --workspace` — all clean.
2. Graph lane: dogfood retrieval for a known hot-spot file; confirm
   at least one co-edit neighbor lands in the packet's graph lane
   items.
3. Sandbox: `AZOTH_SANDBOX=tier_b AZOTH_PROFILE=openrouter cargo run`
   → bash tool write to `/etc/passwd` must return EACCES without
   terminating the session.
4. Live eval: `cargo run -p azoth -- eval run --live-retrieval . --seed docs/eval/v2_seed_tasks.json --k 5`
   — localization@5 emits a number ≠ the seed-vs-seed 0.4500; no
   panic; `.azoth/sessions/eval_*_k5.jsonl` is well-formed.

## What this plan does NOT ship

- Full AZOTH_SANDBOX=on as default. That's v2.1.
- Smart graph-lane query extraction (e.g. symbol-resolver-driven
  seed paths). v2.5.
- Live retrieval for symbol/FTS lanes separately (each lane's
  contribution broken out). Current design collapses to composite
  output; per-lane attribution is a dashboard feature, out of v2
  scope.
- Mergeability proxy. Plan already demoted this to research.

## Rollback plan

Each PR is independent. If PR A merges and C regresses, revert C; A
stays. Graph lane (PR B) is guarded by `Option<…>` — setting the
slot back to None restores the pre-PR behaviour without code changes.

—

Approved by user directive "proceed, without exceptions or
hesitations" on 2026-04-18.
