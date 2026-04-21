# Azoth v2.1 → v2.2 → v2.5 Trilogy Design

**Date:** 2026-04-21
**Status:** draft, awaiting user review
**Prior art:**
- `docs/v2_plan.md` (v2 ship plan, superseded trajectory)
- `docs/superpowers/specs/2026-04-18-v2-closure-design.md` (v2 closure PRs)
- `docs/research/deep-research-report.md` §v2.5 and §v3 (visionary scope)
- Current state: `main @ 3a81292`, tag `v2.0.2` (Chronon Plane shipped)

---

## Executive summary

Three sequential minor releases close the explicitly-deferred work from the v2 plan that still meets a real engineering bar, and stop at the v2.5 fence per user decision. **v2.1** adds language breadth (Py/TS/Go tree-sitter + TDAD selectors) and flips the sandbox default to on. **v2.2** ships a gated BGE cross-encoder reranker and TOML tool-level origin-rule enforcement. **v2.5** is a hardening release (bounded red-team harness + session verify/repair + definitive Tier C/D documentation). Total estimated calendar: **~13 weeks**. Every item carries a falsifiable ship criterion; no fuzzy gate verbs.

**What this design deliberately drops (with documented reasons):** LSP (structural v2-plan reasons still hold), Firecracker (no consumer + untestable on primary WSL2 dev env), gix upgrade (no evidence of shell-out insufficiency), mergeability proxy (no PR corpus — stays research), plus all v3 items per strict fence.

---

## Context: what shipped vs what the v2 plan promised

**v2.0.2 shipped** (verified against code, not against claims):
- Hand-rolled migrator (m0001–m0007), no refinery dependency
- FTS5 retrieval (`LexicalBackend::Fts` is the default per `crates/azoth-core/src/retrieval/config.rs`)
- Tree-sitter symbol index (Rust-only)
- Co-edit graph retrieval (git shell-out)
- Composite 4-lane evidence collector (`RetrievalMode::Composite` default)
- TDAD `CargoTestImpact` selector + `ImpactValidator` wiring
- Eval plane: localization@k metric, 20-task seed, `--live-retrieval` flag
- Sandbox Tier-A and Tier-B wired into BashTool with graceful degradation
- Chronon Plane (invariant #8 time-as-taint): Clock injection, bitemporal evidence, TurnHeartbeat, `resume --as-of`, m0007 turns.at index
- PAPER TUI redesign (cards, palette, rail, inspector, GFM markdown, motion, dual-clock)
- OAuth adapter path for Anthropic
- SLSA v1.0 release provenance

**Two unplanned systems** landed during v2 development: Chronon Plane added invariant #8 at real cost (7 review rounds); PAPER TUI took 27 review rounds. Both now load-bearing. They consumed calendar slack the plan had budgeted for v2.1 items, which is why the trilogy below exists.

**Genuinely unshipped deferrals** (reconciled against code, not plan-reading):
- Python / TypeScript / Go tree-sitter grammars (Rust only today)
- pytest / jest / go-test TDAD selectors (cargo only today)
- Sandbox default flip (currently `AZOTH_SANDBOX=Off` on unset)
- BgeReranker cross-encoder (trait stubbed with `unimplemented!()`)
- Origin-rule enforcement DSL (hardcoded today)
- LSP integration (zero wiring)
- gVisor Tier C (returns `EffectNotAvailable`)
- Firecracker Tier D (returns `EffectNotAvailable`)
- Full red-team harness (6–10 cases today; fuzz not in CI)
- Mergeability proxy (no corpus)
- gix / git2 structured git (shell-out today)

---

## Decision history — how α became α'

User asked to "ship the next version according to the plans (2.0.5 with all planned features, in the light of the dynamic the original plan suffered)."

**Scoping sequence:**
1. **Q1: one bundled release or sequential?** User chose `B` — sequential releases.
2. **Q2: horizon strict to v2.5, or opportunistic on v3?** User chose `i` — strict v2.5 fence; all v3 items out.
3. **Three approaches proposed:** α (plan-literal), β (leverage-first pull v2.5 items forward), γ (vertical slice per language per release).
4. **User chose α.**
5. **User then asked for strict validation and a mature decision.** Naive α was retracted; α' produced after five hard checks:
   - **Check 1** — 2.2 calendar realism: LSP's v2-plan defer reasons are structural (invariant #1 cross-turn state, multi-server lifecycle, degraded-mode policy undefined), not engineering-cost. Calendar does not unlock them. **Result:** LSP removed from 2.2.
   - **Check 2** — Firecracker on WSL2: primary dev env relies on fragile nested-KVM. Shipping into untestable environment is a new anti-pattern. **Result:** Firecracker removed from 2.5.
   - **Check 3** — gVisor/Firecracker consumers: no in-tree tool produces `ApplyRemoteReversible` or `ApplyIrreversible` effects. Building sandbox for consumer-less effect classes is infrastructure for empty room. **Result:** gVisor demoted to foundation-only.
   - **Check 4** — "Full red-team" is unfalsifiable without explicit numbers. **Result:** bounded to ≥50 prompt-escape + ≥20 sandbox-escape + 7-day CI-fuzz clean window.
   - **Check 5** — gix upgrade has no evidence shell-out is insufficient. **Result:** removed from 2.1.

**α' (final):**
- **2.1.0** (4 wk, 11 PRs): Py/TS/Go tree-sitter + pytest/jest/go-test TDAD + sandbox-default-on + red-team corpus +20 cases.
- **2.2.0** (4–5 wk, 7 PRs): BgeReranker (gated by measurable improvement) + TOML tool-level origin-rule enforcement.
- **2.5.0** (5 wk, 8 PRs): bounded red-team harness + session verify/repair + Tier C/D definitive documentation + Prometheus metrics.
- Total: ~13 weeks, ~26 PRs.

Memory patterns applied during validation: `feedback_defer_with_scope_vs_silent_skip.md`, `feedback_fuzzy_gate_verbs_enable_false_victory.md`, `pattern_defer_reasons_outlive_calendar_slot.md`, `pattern_verify_subagent_claims_against_code.md`.

---

## Scope and principles (trilogy-wide)

Seven hard rules govern every release:

1. **Additive-only schema.** New `SessionEvent` variants, new fields on `EvidenceItem` / `Edge` / `Symbol` / `Policy` — all `#[serde(default)]`. Never rename, never remove. Pre-release JSONL must replay clean under the new binary.
2. **Zero new invariants unless structurally forced.** Chronon added #8 at real cost. Bar for #9: "no correct behavior achievable without it" — policy DSL does not meet this bar.
3. **Minor-level version names** (2.1.0, 2.2.0, 2.5.0). Each changes defaults — minor per semver.
4. **Per-release ship gate, non-negotiable:** (a) one live dogfood session per new subsystem, (b) `localization@5 ≥ prior baseline`, (c) full `cargo test --workspace` + fmt + clippy green, (d) release notes enumerate every new `SessionEvent` variant, Origin addition, default flip.
5. **Single-concern PR discipline.** Each release is 7–11 PRs. One PR per language-backend pair; one PR per subsystem. Carries forward PR #15 and PR #18 review-round lessons.
6. **No cross-release dependencies.** Users upgrading 2.0.2 → 2.5.0 directly (skipping 2.1 and 2.2) must get a correct runtime. Feature-flag-simulated CI test asserts this.
7. **Every scope item carries a falsifiable ship criterion.** "Full X" and "support Y" are not criteria. Direction + magnitude + pass/fail.

Release-boundary summary:

| Release | Theme | Cal | PRs |
|---------|-------|-----|-----|
| 2.1.0 | Language breadth + safe-by-default | 4 wk | 11 |
| 2.2.0 | Semantic rerank + origin enforcement | 4–5 wk | 7 |
| 2.5.0 | Defense foundation + hardening | 5 wk | 8 |

---

## 2.1.0 — Language breadth + safe-by-default

**Scope:** Py/TS/Go tree-sitter grammars + matching pytest/jest/go-test TDAD selectors + sandbox-default-on + red-team corpus +20.
**Calendar:** 4 weeks.
**PRs:** 11.

### Architecture decisions

- **Language dispatcher** (new): `code_graph/mod.rs` gains `detect_language(path) -> Option<Language>` (extension-based) and `extract_for(lang, path, src) -> Vec<Symbol>` dispatch. Existing Rust path routes through this.
- **SymbolKind extension:** add `Class`, `Method`, `Interface`, `TypeAlias`, `Decorator`, `Package`. Additive variant; serde tag unchanged. Pre-2.1 sessions replay clean.
- **Grammar crates:** `tree-sitter-python = "0.21"`, `tree-sitter-typescript = "0.21"` (exposes both `LANGUAGE_TYPESCRIPT` and `LANGUAGE_TSX`), `tree-sitter-go = "0.21"`. Compatibility with workspace `tree-sitter = "0.22"` verified in PR 2.1-A; workspace upgrade possible if any grammar needs 0.23+.
- **Per-language query files:** `crates/azoth-repo/queries/{python,typescript,go}.scm` capture top-level declarations + one level of nesting (class methods).
- **TDAD TestRunner completion:** `PytestRunner`, `JestRunner`, `GoTestRunner` implement existing `TestRunner` trait. Runners shell out via `BashTool` code path (respects `AZOTH_SANDBOX`).
- **Jest scope fence:** single-project only; monorepos return typed `UnsupportedConfig`. Revisit post-trilogy.
- **Python scope fence:** runner expects resolved deps (user pre-ran `pip install -e .` or equivalent); returns typed `DependenciesUnresolved` cleanly on failure.
- **JavaScript grammar**: **not in 2.1** (.js / .jsx / .mjs / .cjs produce no symbols). TypeScript-only breadth. Revisit later.

### PR sequence and dependencies

```
  A ──┬─> B ──> E ──┐
      ├─> C ──> F ──┤
      └─> D ──> G ──┤
                    ├─> J ──> K
  H ───────────────>┤
  I ───────────────>┘
```

### PR-by-PR ship criteria

**2.1-A — SymbolKind extension + language dispatcher.**
Extend enum; add `Language`; add dispatcher. Existing Rust extractor routes through dispatcher.
*Ship:* full test suite green; pre-2.1 JSONL + SQLite replay clean (`tests/v2_1_forward_compat.rs`); dispatcher returns correct `Language` for 20 path fixtures across 4 languages.

**2.1-B — Python tree-sitter.**
`code_graph/python.rs` + `queries/python.scm`. Extracts Function, Class, Method, Decorator, Module.
*Ship:* on 500-LOC fixture ≥90 0eclared functions/classes/methods extracted; <50ms per file <1000 LOC; incremental reindex re-parses only changed files (mtime gate); no panic on malformed syntax.

**2.1-C — TypeScript tree-sitter.**
`code_graph/typescript.rs` + `queries/typescript.scm`. Handles both `.ts` (LANGUAGE_TYPESCRIPT) and `.tsx` (LANGUAGE_TSX). Extracts Function, Class, Method, Interface, TypeAlias, Enum.
*Ship:* same bar as B on a 500-LOC fixture including one .tsx; dispatcher routes correctly per extension.

**2.1-D — Go tree-sitter.**
`code_graph/go.rs` + `queries/go.scm`. Extracts FuncDecl, MethodDecl, TypeDecl, ConstDecl, Package.
*Ship:* same bar as B on a 500-LOC Go fixture.

**2.1-E — pytest TDAD.**
`PytestImpact` + `PytestRunner`. Detection: `pytest.ini` or `pyproject.toml[tool.pytest]` or `setup.cfg[tool:pytest]`. Heuristic edges: src/foo.py → tests/test_foo.py + tests/**/test*foo*.py + symbol-graph callers + co-edit neighbors.
*Ship:* on seed pytest fixture (≥10 src + ≥10 test files), selector proposes ≥1 relevant test for single-file diffs in ≥800f cases; runner returns correct pass/fail matching raw `pytest` invocation; `DependenciesUnresolved` returned cleanly on missing deps.

**2.1-F — jest TDAD.**
`JestImpact` + `JestRunner`. Detection: `jest.config.{js,ts,mjs,cjs}` or `package.json[jest]`. Same heuristic pattern as pytest.
*Ship:* same bar as E on jest fixture. Monorepo configs return `ImpactError::UnsupportedConfig`.

**2.1-G — go test TDAD.**
`GoTestImpact` + `GoTestRunner`. Detection: `go.mod`. Go convention: same-dir `_test.go`.
*Ship:* same bar as E on Go module fixture; package-path resolution correct for `go test -run` invocation.

**2.1-H — sandbox default flip.**
Change default when `AZOTH_SANDBOX` unset from `Off` to `TierA`. Audit all tests that exercise bash without setting the env. Graceful degradation on non-Linux or missing `CLONE_NEWUSER` returns Off with `tracing::warn`.
*Ship:* `cargo test --workspace` green under flipped default; new test `default_sandbox_is_tier_a` asserts it; opt-out `AZOTH_SANDBOX=off` preserved; release notes prominently document the flip + opt-out.
*Contingency:* if >10 tests break, PR splits into H1 (audit + fix keeping Off default) and H2 (flip); H2 merges only when H1 green.

**2.1-I — red-team corpus +20.**
5 categories, 4 cases each: path-traversal, unicode-normalize, FTS5 snippet with embedded prompt-escape, symbol names with shell metacharacters, origin-spoofing.
*Ship:* 20 cases in `tests/v2_injection_surface.rs`; each asserts explicit block/sanitize/quarantine outcome; inline justification per case.

**2.1-J — dogfood runs + eval seed expansion.**
Three live sessions (Py, TS, Go) on real public projects; transcripts archived to `docs/dogfood/v2.1/`. Eval seed grown to 50 tasks (20 Rust + 10 Py + 10 TS + 10 Go) at `docs/eval/v2.1_seed_tasks.json`.
*Ship:* `localization@5 ≥ 0.45` on expanded seed (matches prior baseline); each dogfood session emits evidence-lane entries tagged with the new language symbol lane; zero new `turn_aborted` variants in the three sessions.

**2.1-K — version bump + release notes + tag.**
`workspace.version = "2.1.0"`. CHANGELOG enumerates: new `SymbolKind` variants, new TDAD backends, `AZOTH_SANDBOX` default flip, explicit non-scopes (jest monorepo, .js, LSP, gix), 20 new red-team cases.
*Ship:* annotated tag `v2.1.0`; release workflow green with SLSA v1.0; no `unimplemented!()` newly introduced on public paths.

### Top risks (2.1-specific)

1. **Tree-sitter grammar version drift.** Mitigation: PR A compat-matrix check; workspace upgrade possible.
2. **Sandbox-default-on audit reveals >10 broken tests.** Mitigation: H1/H2 split path.
3. **Jest monorepo variance breaks real-project dogfood.** Mitigation: single-project dogfood target + typed error for monorepos.
4. **Python dep-resolution fragility.** Mitigation: documented expectation + typed `DependenciesUnresolved`.
5. **Per-language grammar timeline mismatch** (TypeScript hardest due to .tsx + type/value namespaces). Mitigation: independent PRs; slowest language may slip to 2.1.1 without blocking others.

### Verification at release

- `cargo test --workspace` green (expect ~50 new tests)
- 3 archived dogfood transcripts
- `localization@5 ≥ 0.45` on 50-task seed
- `cargo clippy -D warnings` + `cargo fmt --check` clean
- Release notes enumerate every public-surface change

---

## 2.2.0 — Semantic rerank + origin enforcement

**Scope:** BgeReranker cross-encoder (gated by ≥0.05 localization@5 improvement) + TOML tool-level origin-rule enforcement. LSP explicitly excluded.
**Calendar:** 4–5 weeks.
**PRs:** 7.

### Architecture decisions

- **D1 — Reranker backend: `ort` + BGE-reranker-v2-m3 INT8 quantized.** Cross-encoder, ~150MB quantized; 16-batch inference on CPU ≈ 150–250ms for 50 candidates. `candle` rejected (less mature for this model class); `rust-bert`/tch rejected (LibTorch dep bloats binary). New dep: `ort = "2.0"` with `load-dynamic` feature.
- **D2 — Model distribution: opt-in download, not bundled.** `azoth model fetch bge-reranker-v2-m3` downloads to `~/.azoth/models/`. Default `reranker.backend = "rrf"` preserves offline-build correctness.
- **D3 — Policy grammar: TOML tool-level rules only.** Field-level origin tracking requires rewriting every `Tool::Input` struct (~60 sites) or a proc-macro — out of 2.2 scope. Tool-level grammar: `(tool_name × origin) → {Allow, Deny, RequireApproval}`. First-match-wins. Default policy permissive with explicit, justified denies.
- **D4 — Enforcement point: dispatcher seam.** `ErasedTool::dispatch` already owns extraction + taint gate (CRIT-2). Policy check slots in post-extraction, pre-execute. Emits `SessionEvent::PolicyEvaluated { turn_id, tool, origin, effect, rule_name }` for observability — valuable even on Allow.
- **D5 — Forward-compat: reserved `field` key errors out.** TOML rules containing `field = "..."` are rejected in 2.2 parser with message "field-level rules are v3+; remove `field` or upgrade when v3 ships." Prevents users writing rules that 2.3 interprets differently.

### PR sequence and dependencies

```
  A ─> B ─> C ────┐
                  ├─> G
  D ─> E ─> F ────┘
```

Two independent tracks (reranker A/B/C and policy D/E/F) converge at G.

### PR-by-PR ship criteria

**2.2-A — Reranker benchmarking harness + RRF baseline.**
New `azoth bench reranker --seed v2_seed` subcommand runs seed tasks through each backend (Identity / RRF / stub-BGE), computes localization@5 per backend, prints comparison table.
*Ship:* on 50-task post-2.1 seed, all three backends produce non-zero localization@5; baseline number published in CHANGELOG for forward reference.

**2.2-B — BgeReranker `ort` implementation.**
`BgeReranker::score(query, items)` batches by `reranker.batch_size` (default 16), uses INT8 quantized model. First call pays ~3–5s init; subsequent calls amortize. `azoth model fetch bge-reranker-v2-m3` downloads + verifies SHA256.
*Ship:* 10 unit tests (batched inference, empty input, init failure fallback); p95 latency for 50-item batch < 500ms on 8-core CPU; missing model file returns typed `RerankError::ModelNotFound`, not panic.

**2.2-C — Gate: BGE vs RRF on 50-task seed.**
Integration test runs full seed through both backends, asserts `localization@5(BGE) ≥ localization@5(RRF) + 0.05`.
*Ship — the hard gate:*
- If assertion holds: BGE ships as **opt-in** (config knob surfaced, documented; default stays RRF). Default flip deferred to 2.3+ pending more field evidence.
- If assertion fails: BGE ships behind `cfg(feature = "bge_reranker")` only, documented as experimental. Default stays RRF.
- No fuzzy gate verb: `≥ baseline + 0.05` is direction + magnitude + falsifiable.

**2.2-D — Policy types + TOML parser + default policy file.**
`crates/azoth-core/src/authority/policy.rs` — `Policy { rules: Vec<Rule>, default_effect: Effect }`, `Rule { name, tool_matcher, origin, effect, reason }`, `Effect ∈ {Allow, Deny, RequireApproval}`. TOML → serde. Ship `default-policies.toml` at repo root with 3–5 opinionated defaults (`WebFetch → bash = deny`, `ModelOutput → write = require_approval`, etc).
*Ship:* `tests/policy_parse.rs` covers 10 rule variants + 5 malformed inputs (typed error); round-trip serde preserves semantics; `PolicyLoader` merges project `.azoth/policies.toml` over default (project first, first-match-wins); parser rejects any rule containing `field = "..."` with forward-compat error.

**2.2-E — Enforcement at dispatcher + `PolicyEvaluated` events.**
Dispatcher calls `policy.evaluate(tool_name, tainted.origin())` post-extraction. Emits event always; returns `ToolError::PolicyDenied { rule, reason }` on deny; yields to authority engine on RequireApproval.
*Ship:* 6 integration tests cover `{Allow, Deny, RequireApproval} × {matching rule, no match}`; every dispatch emits exactly one `PolicyEvaluated` event with rule name resolved; `PolicyDenied` error carries rule name + reason.

**2.2-F — Default-policy dogfood + documentation.**
Run 2.1's eval seed (50 tasks) against default policy. Expected: zero new `PolicyDenied` vs 2.1 baseline. Any default rule blocking a legitimate call gets revised or demoted to `RequireApproval`.
*Ship:* dogfood produces zero new tool failures; CHANGELOG lists each default rule + justification; `docs/policies.md` describes rule schema + override pattern + one worked example of a user-added strict rule.

**2.2-G — Version bump + release notes + tag.**
`workspace.version = "2.2.0"`. CHANGELOG enumerates: `PolicyEvaluated` event, BGE disposition (opt-in or feature-gated), default-policies.toml location, reserved-`field`-rejection semantics for forward-compat.
*Ship:* annotated tag `v2.2.0`; release workflow green with SLSA v1.0.

### Top risks (2.2-specific)

1. **BGE doesn't beat RRF by +0.05.** Entire reranker track becomes shelf hardware. Mitigation: PR C gate disposition cleanly; seed expansion (100–200 tasks) if signal is ambiguous.
2. **Default policy breaks in-use workflows.** Mitigation: PR F dogfood gate asserts zero new `PolicyDenied`; any deny revised.
3. **ort native-dep fragility across platforms.** Mitigation: `ort` `load-dynamic` auto-download; fallback to RRF on init failure.
4. **Policy-eval hot-path cost.** At 100 rules × 10 tools × 1 call/turn = 1000 comparisons — sub-microsecond. Document 1000-rule soft limit for user configs.
5. **Field-level forward-compat confusion.** Mitigation: parser rejects reserved `field` key with clear message.

### Verification at release

- `cargo test --workspace` green (expect ~25 new tests)
- BGE gate resolved (opt-in OR feature-gated, documented)
- Policy dogfood: 0 new `PolicyDenied` vs 2.1 baseline
- `localization@5 ≥ max(0.45, v2.1 baseline)` on default (RRF) backend
- SLSA v1.0; `cargo clippy -D warnings` + `cargo fmt --check` clean

---

## 2.5.0 — Defense foundation + hardening

**Scope reframe** (from α-prime validation): "gVisor foundation-only" alone is too thin for a minor release (~1 week of work). 2.5.0 becomes a hardening release: bounded red-team harness + session robustness + Tier C/D definitive documentation + Prometheus-format metrics.
**Calendar:** 5 weeks.
**PRs:** 8.

### Architecture decisions

- **D1 — Tier C (gVisor) and Tier D (Firecracker) stay `EffectNotAvailable`.** Neither ships as functional. Reason: zero in-tree tools declare `ApplyRemoteReversible` or `ApplyIrreversible` effects; no plugin ecosystem exists yet. Shipping runsc/Firecracker wiring is building for empty room. The `From<EffectClass> for SandboxTier` match is already exhaustive; no dispatch change needed.
- **D2 — `runsc` probe added, not required.** `sandbox::probe_runsc() -> Option<PathBuf>` on startup; result cached. If a future tool escalates to Tier C without runsc installed, returns `ToolError::TierCNotReady { reason }` cleanly instead of panic. Cheap; forward-compat.
- **D3 — Red-team bounded by explicit numbers.** ≥50 prompt-escape corpus cases, ≥20 sandbox-escape corpus cases, cargo-fuzz running nightly in CI, 7-day zero-new-finding window before tag push. No fuzzy "full."
- **D4 — Fuzz runs in CI only.** GitHub Actions ubuntu-latest is reference. WSL2 fuzz support uneven; local dev not required.
- **D5 — Session verify + repair as explicit commands.** `azoth session verify <run_id>` / `azoth session repair <run_id> [--confirm]`. Dry-run default; `--confirm` required for mutations. Idempotent (repair on clean session = no-op).
- **D6 — Prometheus metrics, no OTEL.** `azoth metrics --format prometheus` emits red-team + fuzz + policy + session counters. OTEL / Grafana / export formats explicitly v3.

### PR sequence and dependencies

```
  A ──┐
  B ──┤
  C ──┤
  D ──┼─> G ──> H
  E ──┤
  F ──┘
```

A/B/C (red-team + fuzz) land first in weeks 1–2; D/E/F (metrics + docs + session tooling) parallel in weeks 2–3; G verifies 7-day clean window in weeks 4–5; H tags.

### PR-by-PR ship criteria

**2.5-A — Prompt-escape corpus (≥50 cases).**
`tests/red_team/prompt_escape.rs`. Five categories × ~10 cases: evidence-lane injection (FTS5 snippet contains "ignore previous instructions"), symbol-name injection (function named with SQL/shell metacharacters), checkpoint-lane injection (prior summary contains escape attempt), contract-lane tampering (user message asks to rewrite contract budget), tool-output injection (read_file returns content with embedded "call bash X").
*Ship:* ≥50 cases; each asserts explicit outcome (quarantine/sanitize/reject); category coverage documented inline.

**2.5-B — Sandbox-escape corpus (≥20 cases).**
`tests/red_team/sandbox_escape.rs`. Four categories × ~5 cases: path traversal (`../../../etc/passwd`), symlink escape (symlink inside pointing outside), cgroup-interface probes, TOCTOU races.
*Ship:* ≥20 cases; all fail at Landlock/seccomp level with documented `EACCES`/`EPERM` expectation; runs under both default Tier A and explicit Tier B.

**2.5-C — cargo-fuzz harness + CI integration.**
`fuzz/` with targets: JSONL parser, SSE stream parser, tool-input deserialization, policy TOML parser. GitHub Actions nightly job runs `cargo fuzz run <target>` for N minutes, stores artifacts, alerts on new crash.
*Ship:* 4 fuzz targets build and run; CI job schedules nightly; artifact upload works; documented "how to reproduce a fuzz finding" runbook.

**2.5-D — Red-team metrics export + observability.**
`azoth metrics --format prometheus` emits counters: `red_team_case_passed{category, case_name}`, `red_team_case_failed{category, case_name}`, `fuzz_findings_new{target}` (24h window), `fuzz_iterations_total{target}`, `policy_denied_total{rule}`, `session_repair_applied_total`.
*Ship:* prometheus text-format validates against `prom2json`; 10-case smoke test covers counter increment/labels; no perf regression on eval seed (localization@5 ≥ prior baseline).

**2.5-E — Tier C/D definitive documentation.**
`docs/architecture/sandbox-tiers.md`: what each tier enforces + doesn't; why Tier C (gVisor) and Tier D (Firecracker) remain `EffectNotAvailable`; structural criteria that would unlock each (named consumer + dev-env testability). Explicitly cites `pattern_defer_reasons_outlive_calendar_slot.md` rationale.
*Ship:* doc reviewed, linked from README + CLAUDE.md; includes "when to reconsider" per tier; no prose enabling future calendar-pull.

**2.5-F — Session verify + repair subcommands.**
`azoth session verify <run_id>`: scan JSONL for dangling turns, orphaned `tool_result` without matching `tool_use_id`, schema drift. Human-readable report.
`azoth session repair <run_id>`: apply safe fixes (synthetic `turn_interrupted { reason: "crash" }` for dangling turns), requires `--confirm`.
*Ship:* three integration tests: (i) corrupted fixture with 3 dangling turns → verify reports all 3; repair heals all 3; post-repair verify returns clean; (ii) orphaned tool_result fixture → verify flags; repair truncates turn; (iii) `--confirm` gate — default dry-run.

**2.5-G — 7-day clean window + release-gate workflow.**
GitHub Actions release-gate workflow: on tag candidate, refuses tag push unless last 7 consecutive nightly fuzz runs show zero new crashes. Sourced from artifact storage.
*Ship:* workflow blocks synthetic "day-6 crash" in history; unblocks on 7-day clean; documented override for emergency release (requires maintainer acknowledgment).

**2.5-H — Version bump + release notes + tag.**
`workspace.version = "2.5.0"`. CHANGELOG enumerates: red-team corpus composition (cases per category), fuzz targets, session commands, Tier C/D clarification, prometheus metrics, every new `SessionEvent` (`RedTeamCaseExecuted`, `FuzzFindingReported`, `SessionRepairApplied`).
*Ship:* annotated tag `v2.5.0`; release workflow green with SLSA v1.0; 7-day clean window satisfied.

### Top risks (2.5-specific)

1. **Fuzz finds real crash during 7-day clean window.** Release slips ≥7 days per reset. Mitigation: schedule 2.5's clean window in weeks 4–5 of 5-week release; fixes flow into weeks 3–4. Don't bypass the gate.
2. **Red-team uncovers P0 invariant violations in 2.1/2.2 already tagged.** Mitigation: triage as 2.5.x patch vs 2.5.0 forward-only fix case-by-case; CVE-class gets backport.
3. **Session-repair over-reaches.** Mitigation: default dry-run; `--confirm` required; `verify-after-repair` idempotent.
4. **Prometheus scope-creep.** Mitigation: v2.5 ships only Prometheus text format + named counters; OTEL/Grafana explicit v3.
5. **Tier C/D doc re-pulled by future Claude despite pattern memory.** Mitigation: doc names pattern-memory file + links α→α-prime decision log; pattern memory cites both LSP and Firecracker as worked examples. Triple-cited.

### Verification at release

- `cargo test --workspace` green including `tests/red_team/*`
- Last 7 CI nightly fuzz runs: zero new crashes
- `azoth session verify` + `azoth session repair` demonstrated on corrupted fixture
- `docs/architecture/sandbox-tiers.md` exists + linked
- `localization@5 ≥ prior baseline` (no regression from metrics export)
- Release notes enumerate all changes + explicitly state what did NOT ship (Tier C/D implementations) and why

---

## Cross-cutting discipline (trilogy-wide)

### A. Data-plane discipline

**Schema forward-compat is the trilogy's first commandment.** Every `SessionEvent` addition is `#[serde(tag = "type", rename_all = "snake_case")]` compatible; every new field uses `#[serde(default = "...")]`. Each release ships `tests/v{N}_forward_compat.rs` loading a fixture from the prior release. CI gate: PRs touching `schemas/`, `retrieval/`, `authority/policy`, or `red_team/` must update the forward-compat test.

**Eval gate is measured, not claimed.**
- Baseline: established in PR 2.2-A on 50-task post-2.1 seed using RRF reranker.
- Per-release target: `localization@5(release) ≥ max(0.45, prior_release_baseline)`. Never regress.
- BGE gate (2.2 only): `localization@5(BGE) ≥ localization@5(RRF) + 0.05` or BGE does not become default.
- Seeds versioned per release: `docs/eval/v2.1_seed_tasks.json`, `v2.2_seed_tasks.json`, etc. Baselines comparable only on same seed.
- Failed eval gate blocks release tag. No "fix next version" escape.

### B. External-facing discipline

**Dogfood minimums** (non-negotiable pre-tag):
- 2.1: 3 live sessions (Py, TS, Go), transcripts archived to `docs/dogfood/v2.1/`.
- 2.2: 2 live sessions (default RRF+policy, opt-in BGE if gate passes).
- 2.5: 1 live session + `azoth session verify` on a corrupted fixture + one fuzz-finding triage exercise.

**Doc-as-code rule:** every PR changing behavior updates docs in the same PR. No "docs follow-up." Per-release required updates: `CLAUDE.md`, `README.md`, `docs/draft_plan.md` scope-fence trim, `docs/v2_plan.md` sprint-shipped marking, `CHANGELOG.md` enumeration. New per-release docs: 2.1 → `docs/tdad-per-language.md`; 2.2 → `docs/policies.md`; 2.5 → `docs/architecture/sandbox-tiers.md` + `docs/red-team-corpus.md`.

### C. Process discipline

**Commit identity:** `git -c user.email=dev.ungheni@gmail.com -c user.name=nalyk`. No Claude authorship; no "Generated with Claude Code"; no Co-Authored-By. (Applied from memory rule across the trilogy.)

**PR granularity:** single-concern per PR. Title: `azoth: <rel> <pr-letter> — <description>`. Body names scope + ship criterion + risks. Both review bots (gemini + codex) invoked per merge candidate. Review rounds target ≤3, cap at 5 with self-audit each round.

**Release cadence:** final PR per release bumps `workspace.version`, updates CHANGELOG, gates on annotated-tag release workflow with SLSA v1.0. Tag on main's SHA, not a release branch.

**Memory hygiene:** one project-status memory per tagged release; supersede prior in-progress pointers. MEMORY.md index line under 200 chars. After each release, prune ≥2 superseded status pointers >30 days old. Extract reusable patterns per release (candidates already seen: verify-subagent-claims, defer-reasons-outlive-calendar-slot).

### D. Architectural discipline

**Target: zero new invariants across the trilogy.** Bar for #9: "no correct behavior achievable without it." Policy DSL fails this bar (rules are opinions; default-permissive proves feature is orthogonal, not foundational). If 2.2 dogfood shows policy is load-bearing in practice, that's a 2.3+ conversation.

**Cross-release independence rule 6 — tested, not promised:**
- CI job `cargo test --workspace --features simulate_skip_2_1_2_2` mocks a 2.0.2 → 2.5.0 direct upgrade.
- All tools gracefully degrade: Tier C/D stay `EffectNotAvailable`; policy loads default-permissive if no user config; missing BGE model falls back to RRF; red-team corpus runs against 2.0.2-era code paths.
- Asserts: zero unimplemented-panic, zero schema-failure, localization@5 measurable.

### E. Test-pattern inheritance

All new tests follow CLAUDE.md `## Test Patterns`:
- `--test-threads=1` for TUI + sandbox + backpressure tests.
- `tempfile::TempDir` returned alongside `PathBuf` (drop-order trap).
- 17-byte chunk splits for stream/SSE parser tests.
- `MockAdapter` + `MockScript` for headless TurnDriver; `wiremock` for live HTTP.
- Red-team fuzz in CI only (ubuntu-latest).

### F. What cross-cutting does NOT include

Honest exclusions to prevent scope-creep poisoning per release:
- Performance CI regression gate: post-2.5 work.
- OTEL / Grafana / observability-as-product: explicit v3.
- Provider-routing / multi-adapter orchestration: explicit v3.
- Plugin SDK / external tool registration: v3.

---

## Consolidated risk ledger

### S0 — Release-blocker

**R1. Calendar overrun past ~26 weeks.** PR #15 = 27 rounds; PR #18 = 7 rounds. 2× worst-case realistic.
- *Mitigation:* re-run `superpowers:brainstorming` at each release start — spec is a map, not territory. If 2.1 actual > 1.5× estimate, stop and re-plan.
- *Signal:* per-release post-mortem memory compares estimated vs actual.

**R2. Single-person bandwidth / review fatigue.** User + AI solo driver; 27-round PR demonstrated real cost.
- *Mitigation:* each release has an explicit MVP variant — 2.1 MVP = "2 languages + sandbox default-on"; 2.2 MVP = "policy DSL, no BGE"; 2.5 MVP = "red-team corpus + session-repair, no fuzz CI". If any PR exceeds 7 review rounds, activate MVP variant.

**R3. Scope creep by habit.** Pulling 9 items forward from v2.1/v2.5 normalizes "one more thing."
- *Mitigation:* rule 6 (no cross-release deps) + cross-cutting §F fence + rule 7 (falsifiable ship criterion). Item without all three gates back out.
- *Signal:* PR body without falsifiable ship criterion blocks on self-review.

### S1 — Release-delayer

**R4.** Sandbox-default-on audit >10 broken tests. *Mitigation:* 2.1-H1/H2 split.
**R5.** BGE doesn't beat RRF by +0.05. *Mitigation:* PR 2.2-C disposes cleanly (opt-in or feature-gated); honest negative-result CHANGELOG.
**R6.** Fuzz finds crash during 7-day clean window. *Mitigation:* schedule clean window weeks 4–5; weeks 1–3 for corpus/fuzz/fixes; don't bypass gate.
**R7.** Red-team uncovers P0 invariant violations in 2.1/2.2 already tagged. *Mitigation:* 2.5.x patch path or 2.5.0 forward-only fix, case-by-case.

### S2 — Feature-compromiser

**R8.** Tree-sitter grammar version drift. *Mitigation:* PR 2.1-A compat-matrix check; workspace upgrade possible.
**R9.** Per-language grammar timeline mismatch. *Mitigation:* PRs 2.1-B/C/D independent; slowest slips to 2.1.1.
**R10.** Jest monorepo variance. *Mitigation:* typed `UnsupportedConfig`; dogfood target matches fence.
**R11.** Python dep-resolution fragility. *Mitigation:* typed `DependenciesUnresolved` + documented expectation.
**R12.** `ort` native-dep fragility. *Mitigation:* `load-dynamic` auto-download; fallback to RRF on init failure.
**R13.** Default policy breaks workflows. *Mitigation:* PR 2.2-F dogfood asserts zero new `PolicyDenied`.
**R14.** Session-repair over-reach. *Mitigation:* dry-run default; `--confirm` required; idempotent verify-after-repair.

### S3 — Tech debt accumulators

**R15.** Invariant accumulation pressure for #9. *Mitigation:* zero-new-invariants target; 2.3+ conversation if truly needed.
**R16.** MEMORY.md unbounded growth (already 56KB / 24KB limit). *Mitigation:* per-release trim discipline; index entries <200 chars.
**R17.** Tier C/D doc re-pulled by future session despite pattern memory. *Mitigation:* doc triple-cites pattern memory + α→α-prime decision log + both worked examples.
**R18.** Field-level rule forward-compat confusion. *Mitigation:* 2.2 parser rejects reserved `field` key with clear message.

### S4 — Plan-quality risks

**R19.** Research-doc items missed. *Mitigation:* explicit reconciliation section below.
**R20.** Spec becomes authority that propagates blind spots. *Mitigation:* explicit "Assumptions" + "What I didn't investigate" sections below.
**R21.** External dep churn (ort, tree-sitter-*, fuse-overlayfs, runsc). *Mitigation:* pin versions; explicit changelog review per `cargo update`.
**R22.** Dogfood sessions synthetic / degraded. *Mitigation:* real public projects named (candidates: `requests` Py, small TS lib, small Go CLI); transcripts committed.
**R23.** `simulate_skip_2_1_2_2` CI feature may not catch real drift. *Mitigation:* concrete implementation in 2.5; if gap found, document + don't fake green.

### The one meta-mitigation

**Each release re-brainstorms at start.** This spec is ground truth for today (2026-04-21). It will drift. Chronon and PAPER were unplanned because v2 plan was treated as gospel past shelf life.

**Gate discipline:**
- Before starting 2.1: re-read spec, name what changed, update if needed, then invoke `superpowers:writing-plans` for implementation plan.
- Before starting 2.2: same drill. Dogfood from 2.1 may surface that LSP is cheap now or that field-level policy is urgent. Re-validate, don't execute stale.
- Before starting 2.5: same.

Three re-brainstorm checkpoints beat one giant plan executed without re-validation. The structure is the risk mitigation.

---

## Explicitly dropped — items not shipping in the trilogy + why

Each item below was considered and deliberately cut. Recording rationale here prevents future sessions from reviving without reason-dissolution (per `pattern_defer_reasons_outlive_calendar_slot.md`).

**LSP (full).** v2 plan's three defer reasons remain structurally unresolved:
- *Turn-atomicity conflict.* Rust-analyzer (and most LSP servers) hold cross-turn file buffers + semantic state. Invariant #1 ("transcript is not memory") forbids this. Resolving means amending #1 (bigger than the feature) or stripping LSP to stateless queries (~70% value lost).
- *Multi-server lifecycle.* Each language = one server = one process to spawn/manage/kill/recover. Degraded-mode policy undefined.
- *Tree-sitter covers 80% of retrieval value.* 2.1's Py/TS/Go breadth plus 2.2's BGE cross-encoder likely raises the cover toward 90%. Diminishing return on LSP.
- **Revisit when:** a concrete retrieval failure is traced to LSP-specific capability (cross-file def resolution, type inference) that tree-sitter + BGE cannot address, AND invariant-#1 amendment is on the table.

**Firecracker (Tier D).**
- *No consumer.* No in-tree tool produces `ApplyIrreversible` effect class.
- *Primary dev env untestable.* WSL2 nested-KVM support is fragile per-build. Shipping a feature that can't be tested on the primary dev environment is a regression in project discipline.
- **Revisit when:** (a) an `ApplyIrreversible` consumer exists (provider-routed remote tool, probably v3), AND (b) a testable primary dev environment for KVM is established.

**gVisor (Tier C) full implementation.**
- *No consumer.* Same reason as Firecracker. No `ApplyRemoteReversible` tool in tree.
- 2.5 ships Tier C **foundation only** (runsc probe + error message cleanup + docs).
- **Revisit when:** a consumer declares `ApplyRemoteReversible`.

**gix / git2 structured git.**
- *No evidence shell-out is insufficient.* Typed-error loss is acceptable per v2 plan's original §Strategic cuts.
- **Revisit when:** a concrete failure is traced to shell-out (parse ambiguity, race condition, perf bottleneck).

**Mergeability proxy eval metric.**
- *No PR corpus.* Metric requires ground-truth merge-or-not labels across hundreds of real PRs. Building the corpus is weeks of labor; demoted to research followup by v2 plan.
- **Revisit when:** a corpus exists.

**All v3 items** (domain packs, provider routing, enterprise deployment, episodic memory). Out per user's strict-fence decision. Not reconsidered in this spec.

---

## Research-doc reconciliation

`docs/research/` names items that the v2 plan mapped to v2.5 or v3. This spec resolves each against α-prime.

**Research v2.5 items → disposition:**
| Item | In α'? | Rationale |
|------|--------|-----------|
| Taint engine (Origin-rule enforcement) | ✓ 2.2 | TOML tool-level; field-level = v3 |
| Policy DSL | ✓ 2.2 | TOML; honest narrow scope |
| gVisor tier | ⚠ 2.5 foundation | no consumer; full wiring deferred |
| Firecracker tier | ✗ dropped | no consumer + untestable (see above) |
| Red-team suites | ✓ 2.5 | bounded (≥50 + ≥20 + 7-day) |
| Secure deployment mode | ✗ out | v3 (enterprise) |
| Approval automation with bounded trust | ✗ out | distinct from policy DSL; v3-shaped |
| Audit-grade replay manifests | ✗ out | distinct from session-verify/repair; v3 |
| Capability minting (structured DSL) | ✗ out | field-level policy = v3 |

**Research v3 items:** all out per user fence.

**Research-only items** (context-regression evals, mergeability proxy, generational reply compaction, adaptive Context Kernel maturity): stay research. Could stretch into 2.2 eval expansion if dogfood surfaces need, but not in scope by default.

---

## Assumptions I am making (verify at each release start)

This spec encodes beliefs that may be wrong. Each deserves a 30-second check at release-brainstorm time.

1. **Tree-sitter 0.22 + tree-sitter-{python,typescript,go} 0.21 are compatible.** Verified by: trying to build. Failure mode: workspace upgrade needed.
2. **FTS5 default is truly in use on main.** Verified per current code read. Fails if regressed.
3. **`AZOTH_SANDBOX` default flip won't break more than 10 tests.** Unverified. Real blast radius discovered in 2.1-H audit.
4. **BGE cross-encoder gives ≥0.05 localization@5 improvement.** Unknown. Seed may be too small to signal; PR 2.2-C result may be "opt-in only" or "feature-gated."
5. **Zero in-tree tools today declare `ApplyRemoteReversible` or `ApplyIrreversible`.** Asserted but not re-verified at spec time; verify at 2.5-E doc writing.
6. **WSL2 nested-KVM is unreliable for Firecracker.** Asserted; based on general-knowledge claim. If user's WSL2 build reliably supports nested KVM, Firecracker re-enters scope-discussion (not automatic re-entry — requires decision).
7. **20-task eval seed produces meaningful localization@5 signal.** Current RRF baseline unknown; may need 100+ seed for BGE gate statistical power.
8. **13-week calendar is achievable with single-person + AI bandwidth.** Prior evidence: PR #18 took 7 rounds (bearable); PR #15 took 27 (unbearable). If any release approaches PR #15 intensity, MVP variant activates.
9. **Dogfood target projects (`requests`, small TS lib, small Go CLI) remain suitable** at release time. Upstream projects evolve; pick current candidates at 2.1-J time.

## What I deliberately did not investigate

Naming gaps honestly so future sessions know where re-work may be needed.

- **Exact runtime perf cost of each new retrieval lane** (FTS + symbol + co-edit already live; BGE adds ~250ms). Benchmarks come in 2.2-A, not here.
- **Actual size of sandbox-default-on test-break blast radius.** Audit deferred to 2.1-H.
- **Real-world jest monorepo distribution** among likely azoth users. Scope fence stated; quantified need for post-trilogy loosening unknown.
- **Whether BGE-reranker-v2-m3 or a newer rerank model** is better for code retrieval specifically (vs generic text). Assumed BGE-v2-m3 based on multilingual + benchmark strength; 2.2-B may revise.
- **Provider-routing urgency.** Stated v3; research docs emphasize it. Not re-examined against recent dogfood evidence in this spec.
- **Exact corpus composition for red-team categories.** Category counts specified; specific attack vectors enumerated at 2.5-A writing time.
- **Whether `simulate_skip_2_1_2_2` CI feature is implementable as described.** Concept only; engineering feasibility confirmed in 2.5.

---

## Handoff to implementation

**Next action after user approves this spec:** invoke `superpowers:writing-plans` to produce a sprint-level implementation plan **for 2.1.0 only**. Do not produce 2.2/2.5 plans until after 2.1 ships and a fresh brainstorm re-validates.

**Plan inputs:**
- This spec's §2.1.0 section (scope + PRs A–K + ship criteria + risks + verification)
- Current code state at `main @ 3a81292`
- CLAUDE.md architectural constraints (three-crate dep arrow, schema stability rules)
- Memory patterns cited above

**Plan outputs (from writing-plans skill):**
- Per-PR file-touch map
- Per-PR test additions
- Per-PR manual verification checklist
- Sequencing that respects the A → B/C/D → E/F/G + H + I + J + K dependency graph

---

## References

- **Prior azoth specs:**
  - `docs/draft_plan.md` (v1 skeleton)
  - `docs/v2_plan.md` (v2 ship plan)
  - `docs/superpowers/specs/2026-04-18-v2-closure-design.md` (v2 closure)
- **Research docs:**
  - `docs/research/research_00.md`, `research_01.md`, `deep-research-report.md`
- **Memory patterns applied:**
  - `pattern_defer_reasons_outlive_calendar_slot.md`
  - `pattern_verify_subagent_claims_against_code.md`
  - `feedback_fuzzy_gate_verbs_enable_false_victory.md`
  - `feedback_adversarial_self_review_before_push.md`
  - `feedback_defer_with_scope_vs_silent_skip.md`
  - `feedback_reject_with_documentation_when_arch_forbids.md`
  - `project_v2_sprint_gate_discipline.md`
- **CLAUDE.md:** root + `.claude/rules/{crate-boundary,turn-driver,schemas-stability,adapter-protocol}.md`
