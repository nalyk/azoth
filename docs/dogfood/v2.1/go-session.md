# Dogfood — v2.1 Go session

- **Target:** `urfave/cli` @ `b79d76894ba609be305dbb458a95f1bad6ef7f2f`
  (shallow clone, depth=1)
- **Session mode:** headless `azoth eval run --live-retrieval` against the cloned
  repo. Drives the composite pipeline end-to-end from the CLI. The harness
  running this session has no TTY, so an interactive `azoth` TUI run was not
  possible — the headless eval path exercises the same
  `CompositeEvidenceCollector` the TUI worker does.
- **Run id:** `eval_7019441ecb7c_k5_live`
- **Session JSONL:** `.azoth/sessions/eval_7019441ecb7c_k5_live.jsonl`
  (5 `eval_sampled` events, one `run_started`, no aborts, no interrupts).

## Indexer state after reindex

| Metric | Value |
| --- | --- |
| Documents | 147 |
| Symbols | 1232 |
| Language coverage | `go`, `markdown`, `yaml`, untyped |
| Symbols by language | `go=1232` |

All 1232 symbols are Go — PR-D's `tree-sitter-go` extractor (including the
`const_spec.is_named()` filter that closed the comma-leak bug, and the
`method_elem` arm for interface member declarations) resolves the full urfave/cli
package. YAML / Markdown are indexed for FTS only.

## Retrieval probes

| Probe id | Prompt | Relevant | Matched @5 | Considered | precision@5 |
| --- | --- | --- | --- | --- | --- |
| `go_probe_command` | `type Command struct` | `command.go` | 1/1 | 3 | 0.333 |
| `go_probe_flag` | `type BoolFlag` | `flag_bool.go` | 1/1 | 4 | 0.250 |
| `go_probe_completion` | `ShellComplete` | `completion.go` | 1/1 | 5 | 0.200 |
| `go_probe_exit` | `ExitCoder` | `errors.go` | 1/1 | 5 | 0.200 |
| `go_probe_suggestions` | `SuggestCommand` | `suggestions.go` | 1/1 | 5 | 0.200 |

- **Retrieval recall**: 5/5 probes surface the target file in the top-k.
- **Live precision@5 mean**: 0.2367.
- **Lane summary**: `live retrieval: tasks=5 predictions=22 empty=0`.

## Evidence lane observations

Strong recall (5/5) in Go is the clean case: file names match symbol-cluster
purpose tightly (`flag_bool.go` owns `BoolFlag`, `suggestions.go` owns
`SuggestCommand`), so the symbol lane's `by_name` lookup lands directly.
PR-G's TDAD selector and PR-D's extractor feed the same `symbols_by_name`
mirror, so the path set is internally consistent.

Co-edit graph edges: 0 (shallow clone has only 1 commit — the graph build path
walks git log correctly but finds no commit pairs to weight). That's the
expected degradation mode for `depth=1` clones and does not block retrieval;
symbol + FTS + ripgrep lanes carry the signal alone.

As in the Python / TypeScript sessions, the 10 `go_*` entries in
`docs/eval/v2.1_seed_tasks.json` are *seed-mode* entries, scored against
hand-labelled `predicted_files`. v2.1 gate: **seed-mode localization@5 = 0.4783 ≥ 0.45**.

## Turn outcome ledger

- `RunStarted` events: 1
- `EvalSampled` events: 5
- `TurnCommitted` equivalents: all 5 scored without error
- `TurnAborted` / `TurnInterrupted`: **zero new variants**.
