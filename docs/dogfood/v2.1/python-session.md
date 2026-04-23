# Dogfood — v2.1 Python session

- **Target:** `psf/requests` @ `93bf5331a70cd2c77ac3ba43f85c918cae67c69f` (shallow clone, depth=1)
- **Session mode:** headless `azoth eval run --live-retrieval` against the cloned
  repo. This drives the full composite pipeline (graph → symbol → lexical → fts
  → RRF rerank → token-budget) from the CLI without an LLM turn.
  The harness running this session has no TTY, so an interactive
  `azoth` TUI run was not possible — the headless eval path exercises the same
  `CompositeEvidenceCollector` the TUI worker does, and is the measurable
  payload the plan calls for ("lane-tagged evidence for the new language's
  symbols", "zero new `turn_aborted` variants").
- **Run id:** `eval_0564843a1481_k5_live`
- **Session JSONL:** `.azoth/sessions/eval_0564843a1481_k5_live.jsonl`
  (5 `eval_sampled` events, one `run_started`, no aborts, no interrupts).

## Indexer state after reindex

From `state.sqlite` under `<repo>/.azoth/`:

| Metric | Value |
| --- | --- |
| Documents | 94 |
| Symbols | 870 |
| Language coverage | `python`, `markdown`, `toml`, untyped |
| Symbols by language | `python=870` |

All 870 symbols are Python — PR-B's tree-sitter-python extractor wires cleanly
through the v2.1 dispatcher. Markdown / TOML are indexed for FTS but symbol-
extractor is not wired for them (correctly — v2.1 only ships Py/TS/Go).

## Retrieval probes

Five short phrase-style probes run via
`azoth eval run --seed probes-python.json --live-retrieval <clone>`:

| Probe id | Prompt | Relevant | Matched @5 | Considered | precision@5 |
| --- | --- | --- | --- | --- | --- |
| `py_probe_adapter` | `HTTPAdapter` | `src/requests/adapters.py` | 1/1 | 5 | 0.200 |
| `py_probe_session` | `class Session` | `src/requests/sessions.py` | 1/1 | 3 | 0.333 |
| `py_probe_cookies` | `RequestsCookieJar` | `src/requests/cookies.py` | 1/1 | 5 | 0.200 |
| `py_probe_models` | `PreparedRequest` | `src/requests/models.py` | 1/1 | 5 | 0.200 |
| `py_probe_auth` | `HTTPBasicAuth` | `src/requests/auth.py` | 1/1 | 5 | 0.200 |

- **Retrieval recall**: 5/5 probes surfaced the target file in the top-k.
- **Live precision@5 mean**: 0.2267. Low because `k=5` surfaces several neighbour
  files alongside the exact target; the precision-per-task shows the target
  appears in position 1–5 every time.
- **Zero empty lanes**: `live retrieval: tasks=5 predictions=23 empty=0`.

## Evidence lane observations

Natural-language-prose prompts (e.g. the 10-task `py_001..py_010` entries in
`docs/eval/v2.1_seed_tasks.json`) produce zero live-retrieval predictions
because the current FTS5 backend wraps the query as a literal phrase
(`fts5_phrase` in `crates/azoth-repo/src/fts.rs:163`). Short identifier-style
probes match — which is the composite collector's current strong path. This is
an orthogonal finding about the live-retrieval query shape, not a regression in
the Python extractor itself. Scoped as future work for the v2.2 reranker round.

The 10 `py_*` seed entries in `docs/eval/v2.1_seed_tasks.json` are *seed-mode*
entries: their `predicted_files` are hand-labelled, and localization@5 is
measured against `relevant_files` in the same file. That is the canonical v2.1
gate (`cargo test eval_v2_1_seed`) and remains at **0.4783 ≥ 0.45**.

## Turn outcome ledger

- `RunStarted` events: 1
- `EvalSampled` events: 5
- `TurnCommitted` equivalents: all 5 scored without error
- `TurnAborted` / `TurnInterrupted`: **zero new variants** introduced by this
  session — the eval CLI emits the same terminal-marker shape v2 Sprint 6
  already validated.
