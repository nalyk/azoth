# Dogfood — v2.1 TypeScript session

- **Target:** `microsoft/vscode-eslint` @ `93b96ab74540fb07c91eff0c1d614b1d41df4826`
  (shallow clone, depth=1)
- **Session mode:** headless `azoth eval run --live-retrieval` against the cloned
  repo. Drives the composite pipeline end-to-end from the CLI. The harness
  running this session has no TTY, so an interactive `azoth` TUI run was not
  possible — the headless eval path exercises the same
  `CompositeEvidenceCollector` the TUI worker does.
- **Run id:** `eval_ba5042a24403_k5_live`
- **Session JSONL:** `.azoth/sessions/eval_ba5042a24403_k5_live.jsonl`
  (5 `eval_sampled` events, one `run_started`, no aborts, no interrupts).

## Indexer state after reindex

| Metric | Value |
| --- | --- |
| Documents | 122 |
| Symbols | 406 |
| Language coverage | `typescript`, `javascript`, `json`, `yaml`, `markdown`, `shell`, untyped |
| Symbols by language | `typescript=406` |

All 406 symbols are TypeScript — PR-C's `language_typescript()` +
`language_tsx()` dual-factory extractor (including the R1 `function_signature` /
`method_signature` classifier arms) resolves `.ts` and `.tsx` files across the
workspace's `client/`, `server/`, `$shared/`, and `playgrounds/` trees.
JavaScript / JSON / YAML / Markdown / shell are indexed for FTS only (correctly
— v2.1 does not ship JS/JSON/YAML/shell symbol extractors).

## Retrieval probes

| Probe id | Prompt | Relevant | Matched @5 | Considered | precision@5 |
| --- | --- | --- | --- | --- | --- |
| `ts_probe_server` | `class ESLintServer` | `server/src/eslintServer.ts` | 0/1 | 0 | 0.000 |
| `ts_probe_linked` | `LinkedMap` | `server/src/linkedMap.ts` | 1/1 | 2 | 0.500 |
| `ts_probe_customMessages` | `NoESLintLibraryRequest` | `$shared/customMessages.ts` | 1/1 | 3 | 0.333 |
| `ts_probe_settings` | `ConfigurationSettings` | `$shared/settings.ts`, `client/src/settings.ts` | 1/2 | 4 | 0.250 |
| `ts_probe_extension` | `activate` | `client/src/extension.ts` | 1/1 | 5 | 0.200 |

- **Retrieval recall**: 4/5 probes surface the target file in the top-k. The
  miss on `ts_probe_server` is the FTS5-phrase-literality caveat: the source
  declares the server via exported-singleton shape (`export class ESLint` rather
  than `class ESLintServer`), so the literal phrase doesn't appear verbatim.
  Reranker-word-boundary work is scoped to v2.2.
- **Live precision@5 mean**: 0.2567.
- **Lane summary**: `live retrieval: tasks=5 predictions=14 empty=1`.

## Evidence lane observations

The v2.1 TypeScript extractor's `function_signature` + `method_signature`
classifier arms (shipped in PR-C round 1 after codex P1 flags) mean
interface-member and `declare class` declarations enter the symbol lane — which
is why `NoESLintLibraryRequest` (an ambient interface member in
`customMessages.ts`) is findable at all. Without those arms the probe would
return 0.

As in the Python session, the 10 `ts_*` entries in
`docs/eval/v2.1_seed_tasks.json` are *seed-mode* entries, scored against
hand-labelled `predicted_files`. v2.1 gate: **seed-mode localization@5 = 0.4783 ≥ 0.45**.

## Turn outcome ledger

- `RunStarted` events: 1
- `EvalSampled` events: 5
- `TurnCommitted` equivalents: all 5 scored without error
- `TurnAborted` / `TurnInterrupted`: **zero new variants**.
