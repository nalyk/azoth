# Azoth — Architecture Spec (v1 skeleton, greenfield)

## Context

**Why this plan exists.** `/home/nalyk/gits/azoth` is greenfield — only `docs/research/` (research_00.md, research_01.md, deep-research-report.md) exists. The research docs describe Azoth as a contract-centric, context-compiled, event-sourced, provider-agnostic, coding-first CLI agent runtime, phased v1 → v3. The full v1 alone spans six sprints and multiple subsystems; packing it into one plan produces something too coarse to execute. This plan is therefore **architecture-only**: it locks the crate layout, runtime invariants, schemas, module boundaries, internal protocol, sandbox mechanism stack, TUI shape, and data flow — the stable skeleton that v1.5 → v3 will grow into without refactoring. Sprint-level planning is deferred to a follow-up plan once this architecture is approved.

**Decided constraints (user + expert):**
- Rust stable, Linux-only (WSL2 is primary dev target).
- Interactive TUI modeled on Claude Code / Codex CLI / OpenCode — ratatui + crossterm. No daemon. Single binary.
- Coding-first domain; domain packs deferred.
- Two provider adapters from day one: `anthropic-messages` (native) and `openai-chat-completions` (OpenRouter / Ollama `/v1/chat/completions` / vLLM / OpenAI / LiteLLM). Model + endpoint is config.
- Internal model protocol uses **Anthropic Messages content-block shape** (richer; OpenAI adapter downcasts).
- Azoth owns all continuity and caching — no reliance on provider-side stateful chaining (forced by OpenRouter Chat Completions).
- All CRIT/HIGH/MED corrections from the adversarial architecture review are folded in.

## Seven invariants (runtime laws, non-negotiable)

1. **Transcript is not memory.** The Context Kernel compiles per-step packets from durable state; it never appends transcript verbatim.
2. **Deterministic controls outrank model output.** The Authority Engine, validators, capability tokens, and approvals have final authority. Model output cannot override legality.
3. **Every non-trivial run has a contract** with explicit success criteria, scope, and side-effect budget.
4. **Every side effect has a class**, named exactly: `observe` / `stage` / `apply_local` / `apply_repo` / `apply_remote_reversible` / `apply_remote_stateful` / `apply_irreversible`.
5. **Every run leaves structured evidence**: trace events, checkpoints, artifacts, approvals, validator results, replay class, all persisted.
6. **Every subsystem is eval-able** — context kernel, retrieval, tool routing, approvals, validators, contract satisfaction emit measurable signals.
7. **Turn-scoped atomicity.** Every turn has `turn_started` + `turn_committed`/`turn_aborted`/`turn_interrupted` markers. The *replayable* projection drops dangling turns whole; the *forensic* projection retains them. No orphaned `tool_result` blocks ever enter the model's context on resume. (This solves a real bug class seen in prior codebases — mismatched tool_use_id/tool_result when error assistants were skipped but their tool_results were not.)

## Crate layout

```
azoth/                                       (Cargo workspace)
├── Cargo.toml
├── crates/
│   ├── azoth-core/                          # runtime library, zero frontend coupling
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── schemas/                     # serde types: Contract, Checkpoint,
│   │       │                                #   ContextPacket, TraceEvent, EffectRecord,
│   │       │                                #   CapabilityToken, Approval, Turn,
│   │       │                                #   ContentBlock, Message, Role, StopReason,
│   │       │                                #   SecretHandle, Tainted, Origin
│   │       ├── event_store/                 # JSONL session writer/reader (dual projection),
│   │       │                                #   SQLite index via rusqlite + refinery
│   │       ├── artifacts/                   # sha256 content-addressed blob store
│   │       ├── contract/                    # draft, lint, accept (amend deferred)
│   │       ├── context/                     # 5-lane packet compiler, packing rules,
│   │       │                                #   tokenizer family dispatch, cache hints
│   │       ├── retrieval/                   # LexicalRetrieval (ripgrep + FTS5),
│   │       │                                #   GraphRetrieval (stub)
│   │       ├── authority/                   # effect classification, capability tokens,
│   │       │                                #   approval policy (hardcoded v1),
│   │       │                                #   Tainted<T> wrapper + Extractor trait,
│   │       │                                #   SecretHandle
│   │       ├── sandbox/                     # Tier A (user-ns + net-ns + cgroup v2 +
│   │       │                                #   landlock + seccompiler),
│   │       │                                #   Tier B (Tier A + fuse-overlayfs),
│   │       │                                #   Tier C/D hooks (EffectNotAvailable)
│   │       ├── execution/                   # Effect execution, Tool trait,
│   │       │                                #   ToolDispatcher (owns extraction + taint gate),
│   │       │                                #   sandbox tier dispatch
│   │       ├── tools/                       # v1 built-ins (typed input structs per tool)
│   │       ├── adapter/                     # ProviderAdapter trait, ProviderProfile,
│   │       │                                #   AnthropicMessagesAdapter,
│   │       │                                #   OpenAiChatCompletionsAdapter,
│   │       │                                #   AdapterError
│   │       ├── turn/                        # TurnDriver: plan → compile → invoke →
│   │       │                                #   dispatch → validate → commit/abort
│   │       ├── validators/                  # deterministic validator trait + v1 set
│   │       └── telemetry/                   # structured events for eval plane
│   └── azoth/                               # bin crate; default-features = ["tui"]
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs                      # clap entry, dispatch to tui or subcommand
│           └── tui/                         # gated by feature = "tui"
│               ├── mod.rs
│               ├── app.rs                   # AppState, tokio::select! loop (biased)
│               ├── render.rs                # ratatui frame builder
│               ├── widgets/                 # scrollback, turn_block, tool_block,
│               │                            #   approval_modal, evidence_pack,
│               │                            #   status_line, context_indicator
│               └── input/                   # tui-textarea wrapper, slash, @file, history
├── docs/research/                           # existing
├── docs/architecture/                       # this spec + follow-up ADRs live here
├── .azoth/                                  # runtime state, gitignored
│   ├── sessions/<run_id>.jsonl              # append-only turn-scoped event log
│   ├── state.sqlite                         # indexed mirror of committed turns
│   └── artifacts/<sha256>                   # content-addressed blobs
└── examples/
```

**Two crates, not three.** `azoth-tui` as a separate crate is deferred until a second frontend exists. The `tui` module lives behind a default-on feature flag in the `azoth` bin crate. `azoth-core` has zero knowledge of how the frontend renders. This preserves the boundary the three-crate split was chasing, without doubling incremental compile cost while the project has exactly one frontend.

## Core data model (Rust sketch)

### Internal model protocol — Anthropic Messages content-block shape

```rust
pub struct ToolUseId(pub String);
pub struct CallGroupId(pub Uuid);                    // parallel-tool grouping (for OpenAI adapter)

pub enum ContentBlock {
    Text { text: String },
    ToolUse {
        id: ToolUseId,
        name: String,
        input: serde_json::Value,
        call_group: Option<CallGroupId>,             // HIGH-3: preserves OpenAI parallel ordering
    },
    ToolResult {
        tool_use_id: ToolUseId,
        content: Vec<ContentBlock>,
        is_error: bool,
    },
    Thinking { text: String, signature: Option<String> },
}

pub enum Role { User, Assistant }

pub struct Message { pub role: Role, pub content: Vec<ContentBlock> }

pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,             // JSON Schema, not a string
}

pub struct CacheHints {
    pub constitution_boundary: bool,                 // insert Anthropic cache_control here
}

pub struct ModelTurnRequest {
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: u32,
    pub cache_hints: CacheHints,
    pub metadata: RequestMetadata,
}

pub enum StopReason { EndTurn, ToolUse, MaxTokens, StopSequence, ContentFilter }

pub struct ModelTurnResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

pub enum StreamEvent {
    MessageStart,
    ContentBlockStart { index: usize, block: ContentBlockStub },
    TextDelta { index: usize, text: String },
    InputJsonDelta { index: usize, partial_json: String },
    ContentBlockStop { index: usize },
    MessageDelta { stop_reason: Option<StopReason>, usage_delta: UsageDelta },
    MessageStop,
    Error { code: AdapterErrorCode, message: String, retryable: bool },
}

#[async_trait::async_trait]
pub trait ProviderAdapter: Send + Sync {
    fn profile(&self) -> &ProviderProfile;
    async fn invoke(
        &self,
        req: ModelTurnRequest,
        sink: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> Result<ModelTurnResponse, AdapterError>;
    async fn count_tokens(&self, req: &ModelTurnRequest) -> Result<TokenCount, AdapterError>;
}

pub struct ProviderProfile {
    pub id: String,
    pub base_url: String,
    pub model_id: String,
    pub tokenizer_family: TokenizerFamily,           // MED-1: flows into Context Kernel
    pub supports_native_cache: bool,
    pub supports_strict_json_schema: bool,
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
    pub tool_use_shape: ToolUseShape,                // ContentBlock | FlatToolCalls
    pub extra_headers: Vec<(String, String)>,        // e.g. OpenRouter strict header
}

pub enum TokenizerFamily { Anthropic, OpenAiCl100k, OpenAiO200k, SentencepieceLlama }
```

### Authority, taint, secrets

```rust
pub enum Origin { User, Contract, ToolOutput, RepoFile, WebFetch, ModelOutput }

pub struct Tainted<T> { origin: Origin, inner: T }   // no public unwrap
impl<T> Tainted<T> { pub fn origin(&self) -> Origin; pub(crate) fn new(origin: Origin, inner: T) -> Self; }

pub trait Extractor<T, U>: Send + Sync {
    fn name(&self) -> &'static str;
    fn extract(&self, input: Tainted<T>) -> Result<U, ExtractionError>;
}

/// Secret values. Never Serialize. Debug always prints "[REDACTED]".
pub struct SecretHandle(std::sync::Arc<str>);        // MED-2

impl std::fmt::Debug for SecretHandle { /* "[REDACTED]" */ }
// intentionally no Serialize impl; cannot enter ContextPacket evidence lane
```

**CRIT-2 fix — taint lives at the dispatcher seam, not per-tool-input.** The `Tool` trait below takes a tool-specific typed input struct, not raw JSON. The dispatcher performs extraction + taint policy check before calling the tool:

```rust
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    type Input: serde::de::DeserializeOwned + Send;
    type Output: serde::Serialize + Send;

    fn name(&self) -> &'static str;
    fn schema(&self) -> serde_json::Value;
    fn effect_class(&self) -> EffectClass;

    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ExecutionContext,
    ) -> Result<Self::Output, ToolError>;
}

/// Erased wrapper held in the dispatcher's registry.
pub trait ErasedTool: Send + Sync {
    fn name(&self) -> &'static str;
    fn schema(&self) -> serde_json::Value;
    fn effect_class(&self) -> EffectClass;
    fn dispatch<'a>(
        &'a self,
        raw: Tainted<serde_json::Value>,
        ctx: &'a ExecutionContext,
    ) -> futures::future::BoxFuture<'a, Result<serde_json::Value, ToolError>>;
}
// blanket impl<T: Tool> ErasedTool for T performs:
//   1. extraction: Tainted<Value> → T::Input (policy-checked deserialize)
//   2. awaits T::execute
//   3. serializes T::Output
```

Individual tool implementations never see raw model JSON. The taint gate is enforced once, at the boundary, by the blanket impl. Tool authors cannot bypass it.

### Effect classes and sandbox tiers

```rust
pub enum EffectClass {
    Observe,
    Stage,
    ApplyLocal,
    ApplyRepo,
    ApplyRemoteReversible,
    ApplyRemoteStateful,
    ApplyIrreversible,
}

pub enum SandboxTier { A, B, C, D }

impl From<EffectClass> for SandboxTier {
    fn from(ec: EffectClass) -> Self {                // exhaustive — compile error on new variant
        match ec {
            EffectClass::Observe => SandboxTier::A,
            EffectClass::Stage => SandboxTier::B,
            EffectClass::ApplyLocal => SandboxTier::B,
            EffectClass::ApplyRepo => SandboxTier::B,
            EffectClass::ApplyRemoteReversible => SandboxTier::C, // hook only
            EffectClass::ApplyRemoteStateful => SandboxTier::D,   // hook only
            EffectClass::ApplyIrreversible => SandboxTier::D,     // hook only
        }
    }
}
```

## Turn-scoped JSONL session log (dual projection)

Session file: `.azoth/sessions/<run_id>.jsonl`. Append-only. Every line is a self-contained JSON object with a `turn_id`.

```
{"type":"run_started","run_id":"run_abc","contract_id":"ctr_...","timestamp":"..."}
{"type":"turn_started","turn_id":"t_001","run_id":"run_abc","parent_turn":null,"timestamp":"..."}
{"type":"context_packet","turn_id":"t_001","packet_id":"ctx_...","packet_digest":"sha256:..."}
{"type":"model_request","turn_id":"t_001","request_digest":"sha256:...","profile_id":"..."}
{"type":"content_block","turn_id":"t_001","index":0,"block":{"type":"text","text":"..."}}
{"type":"content_block","turn_id":"t_001","index":1,"block":{"type":"tool_use","id":"tu_a","name":"repo.search","input":{...},"call_group":null}}
{"type":"effect_record","turn_id":"t_001","effect":{...}}
{"type":"tool_result","turn_id":"t_001","tool_use_id":"tu_a","is_error":false,"content_artifact":"art_..."}
{"type":"validator_result","turn_id":"t_001","validator":"impact_tests","status":"pass"}
{"type":"checkpoint","turn_id":"t_001","checkpoint_id":"chk_..."}
{"type":"turn_committed","turn_id":"t_001","outcome":"success","usage":{...}}
```

Abort variants record the reason explicitly:

```
{"type":"turn_aborted","turn_id":"t_002","reason":"approval_denied","detail":"user rejected apply_local","usage":{...}}
{"type":"turn_aborted","turn_id":"t_003","reason":"validator_fail","detail":"impact_tests failed","usage":{...}}
{"type":"turn_interrupted","turn_id":"t_004","reason":"user_cancel","partial_usage":{...}}
```

`reason ∈ {user_cancel, adapter_error, validator_fail, approval_denied, token_budget, runtime_error, crash}`. `turn_interrupted` is distinct from `turn_aborted`: interrupted = the turn never had a chance to complete normally (Ctrl+C, crash, adapter stream cut); aborted = the turn ran to a definite negative outcome.

### CRIT-1 fix — dual projection

Two reader projections over the same JSONL file:

- **Replayable projection**: emits only lines from turns whose last marker is `turn_committed`. Dangling turns (no terminal marker), `turn_interrupted`, and `turn_aborted` turns are dropped whole. This is what the Context Kernel reads when rebuilding durable state. **Orphaned `tool_result` blocks are structurally impossible here** — a turn either commits fully or vanishes.
- **Forensic projection**: emits everything, including dangling and interrupted turns, with a `non_replayable: true` annotation. This is what `/status`, postmortem tooling, and the eval plane read. Debugging evidence is preserved.

SQLite mirrors only turns with `turn_committed` or `turn_aborted` (both are *definite* outcomes). `turn_interrupted` and dangling turns live in JSONL only until the user explicitly resolves them.

**Crash recovery**: on session load, the reader scans for turns without a terminal marker. It appends a synthetic `turn_interrupted { reason: "crash" }` record to close them, then proceeds.

## Context Kernel v0 — five lanes

```
ContextPacket
├── constitution_lane    contract digest + tool schemas + policy version   [STABLE PREFIX]
├── working_set_lane     3–7 objects currently in play                      [MUTABLE]
├── evidence_lane        retrieved artifacts, ordered by decision-criticality
├── checkpoint_lane      compact summary of prior committed checkpoint(s)
└── exit_criteria_lane   current step goal + satisfaction rubric
```

**Packing rules**: constitution first (stable prefix, cache key); critical evidence immediately after constitution (not buried mid-packet — Lost-in-the-Middle); exit criteria last; long payloads → artifact refs, never inline; transcript never copied verbatim.

**MED-1 fix — token budgeting is local, not adapter-mediated.** The Context Kernel uses a local tokenizer determined by the active `ProviderProfile.tokenizer_family` (`tiktoken-rs` for OpenAI cl100k/o200k, a local approximation for Anthropic, sentencepiece for Llama family). Packing decisions never make network calls. The `ProviderAdapter::count_tokens()` method is reserved for **pre-flight validation** of the final packet, not for in-loop budgeting. The profile flows into the kernel at compile time of every packet.

**Cache hint**: the `AnthropicMessagesAdapter` places a `cache_control: ephemeral` breakpoint at the end of `constitution_lane`. The `OpenAiChatCompletionsAdapter` ignores cache hints (no native cache; Azoth's own continuity discipline is the substitute).

**HIGH-2 fix — retrieval traits are split from day one:**

```rust
#[async_trait::async_trait]
pub trait LexicalRetrieval: Send + Sync {
    async fn search(&self, q: &str, limit: usize) -> Result<Vec<Span>, RetrievalError>;
}

#[async_trait::async_trait]
pub trait GraphRetrieval: Send + Sync {
    async fn neighbors(&self, node: NodeRef, depth: usize, limit: usize)
        -> Result<Vec<(NodeRef, Edge)>, RetrievalError>;
}
```

v1 ships a `RipgrepFts5Retrieval` impl of `LexicalRetrieval` only. `GraphRetrieval` exists as a trait with a single `NullGraphRetrieval` stub returning `unimplemented!()`. When graph retrieval lands at v2, the Context Kernel acquires it additively — no existing trait signature is touched.

## Authority Engine — hardcoded v1 policy

v1 approval policy matches research §10.4 defaults (no DSL yet):

| Effect class | Approval | Sandbox tier |
|---|---|---|
| `observe` | auto (inside scope) | A |
| `stage` | auto (inside scope, inside effect budget) | B |
| `apply_local` | scoped-once (session-scope upgradable) | B |
| `apply_repo` | manual every time | B |
| `apply_remote_reversible` | **not available in v1** — `EffectNotAvailable` | C (hook only) |
| `apply_remote_stateful` | **not available in v1** — `EffectNotAvailable` | D (hook only) |
| `apply_irreversible` | **not available in v1** — `EffectNotAvailable` | D (hook only) |

Capability tokens are session-scoped, held in `AppState`, serialized to JSONL as events for replay/forensics. Minting happens through approval modals with explicit scope selectors: `once` / `session` / `scoped-paths`.

## Sandbox tiers — honest mechanism stack

### CRIT-3 fix — unprivileged sequence and fuse-overlayfs

All sandbox construction happens in a forked child before tool exec, in this exact order:

```
  1. unshare(CLONE_NEWUSER)    — user namespace first (unprivileged root inside ns)
  2. write /proc/self/{uid_map,gid_map,setgroups}
  3. unshare(CLONE_NEWNET)     — network namespace (loopback-only)
  4. set up cgroup v2 slice via /sys/fs/cgroup/<azoth>/... writes (cpu/mem/pids)
  5. (Tier B only) mount fuse-overlayfs  lower=repo_snapshot upper=tmpfs work=tmpfs
  6. Landlock ruleset apply    — FS allowlist including /sys/fs/cgroup/<azoth>
  7. seccompiler filter apply  — syscall allowlist
  8. execve(tool)
```

**Why `fuse-overlayfs`, not native overlayfs**: native overlayfs `mount(2)` requires `CAP_SYS_ADMIN` in the *initial* user namespace on most kernels. `fuse-overlayfs` runs entirely in userspace via FUSE and works unprivileged. Since WSL2 is the primary dev target and runs without init-ns root by default, `fuse-overlayfs` is the only path that makes Tier B actually function on day one. It is a runtime dependency — the sandbox module probes for the binary on startup and falls back to a degraded Tier B (tmpfs workspace, no diff view) with a visible warning if not present.

**Syscall ordering vs seccomp**: seccomp is the *last* thing applied. Namespace creation, cgroup writes, mount, and Landlock all happen before seccomp because they each need syscalls (`unshare`, `mount`, `prctl`, `openat`, `write`) that the final tool-execution seccomp allowlist may not permit. The allowlist is scoped to the *tool workload*, not the sandbox setup.

**Landlock + cgroup interaction**: `/sys/fs/cgroup/<azoth>/...` is written *before* Landlock apply. After Landlock apply, the process can still read/write its existing cgroup fd because Landlock operates on path lookup, not on already-opened file descriptors. v1 opens the cgroup control files before Landlock apply and keeps the fds for the life of the sandbox.

**Tier C (gVisor) and Tier D (Firecracker)** are architectural hooks only. Effect classes above `apply_local` return `EffectNotAvailable { hint: "scheduled for v2.5" }`. No half-implementation.

## TUI architecture (ratatui 0.30 + crossterm 0.29 + tui-textarea 0.7)

### MED-3 fix — bounded channels and biased select

```rust
// Channels sized for backpressure
let (input_tx,     input_rx)     = mpsc::channel::<InputEvent>(128);
let (model_tx,     model_rx)     = mpsc::channel::<StreamEvent>(64);
let (tool_tx,      tool_rx)      = mpsc::channel::<ToolEvent>(32);
let (authority_tx, authority_rx) = mpsc::channel::<AuthorityEvent>(8);

// Dedicated input task: reads crossterm EventStream, forwards to input_tx.
// Running input on its own task prevents model streaming from starving the
// keyboard reader.

loop {
    tokio::select! {
        biased;                                           // branch priority: input first

        Some(ev) = input_rx.recv()      => state.handle_input(ev),
        Some(ev) = authority_rx.recv()  => state.handle_authority(ev),
        Some(ev) = tool_rx.recv()       => state.handle_tool(ev),
        Some(ev) = model_rx.recv()      => state.handle_model(ev),
        _        = ticker.tick()        => {},
        else => break,
    }
    if state.dirty { terminal.draw(|f| render::frame(f, &state))?; state.dirty = false; }
    if state.should_quit { break; }
}
```

The `biased;` directive plus branch ordering guarantees Ctrl+C is never starved under fast model streaming. The dedicated input task uses a bounded channel with capacity 128 so keyboard bursts do not drop events. Model and tool channels are bounded so a runaway producer applies backpressure instead of consuming unbounded memory.

### Layout v1

```
┌─ azoth · run_abc123 ··························· ctx 37% · deepseek ─┐
│                                                                       │
│  ▸ user: fix the token refresh bug                                    │
│                                                                       │
│  ▼ assistant · turn 001                                               │
│    I'll investigate. Searching for refresh logic.                     │
│    ├─ tool_use  repo.search { "q": "refresh_token" }                  │
│    │  └─ result 4 matches in src/auth/tokens.rs                       │
│    ├─ tool_use  repo.read_spans { ... }                               │
│    │  └─ result <artifact art_f9a4…>                                  │
│    Hypothesis: expiry parsing is off by one second.                   │
│                                                                       │
│  ⧗ awaiting approval · apply_local · src/auth/tokens.rs               │
│    [once]  [session]  [deny]                                          │
│                                                                       │
├───────────────────────────────────────────────────────────────────────┤
│ > _                                                                   │
└───────────────────────────────────────────────────────────────────────┘
```

**Stolen patterns** (per frontend research):
- **Claude Code**: inline-in-scrollback tool blocks with nested results, collapsible turns.
- **Codex CLI**: turn-scoped session files, approval modal overlay.
- **OpenCode**: `ctx N%` indicator in the status line, warning color at ≥80%.
- **Claude Code**: Shift+Enter multi-line, Ctrl+R history search, `/` slash commands, `@` file references.

**Keybindings v1**: `Shift+Enter` newline · `Enter` send · `Ctrl+R` reverse history · `Ctrl+O` focus transcript · `Ctrl+C` cancel current turn · `Esc` dismiss modal · `/` slash menu · `@` file completion · `Ctrl+D` empty-input quit.

**Slash commands v1**: `/contract` · `/approve` · `/status` · `/context` · `/resume` · `/quit` · `/help`. `/replay` and `/export` deferred.

## Data flow — one turn, end to end

1. TUI reads input from `tui-textarea`; on `Enter`, emits `UserInput` to core via the turn channel.
2. `TurnDriver::drive_turn` writes `turn_started`.
3. `ContextKernel::compile(contract, last_committed_checkpoint, step_goal, profile.tokenizer_family)` → `ContextPacket`. `context_packet` event persisted.
4. `TurnDriver` builds `ModelTurnRequest` from the packet + tool registry schemas + cache hints, calls `adapter.invoke(req, model_tx)`.
5. Adapter streams `StreamEvent`s over `model_tx`. `TurnDriver` persists `content_block` events as they complete; the TUI re-renders the turn inline.
6. On `stop_reason: ToolUse`, `TurnDriver` walks the `ToolUse` blocks, grouped by `call_group` (parallel tools execute concurrently within a group, groups serialize). For each:
   - `ToolDispatcher::dispatch(Tainted::new(Origin::ModelOutput, tool_use.input), &ctx)` — extraction + taint gate + schema validation.
   - `AuthorityEngine::authorize(tool.effect_class(), &ctx)` — checks capability token; if missing, pushes `ApprovalRequest` to `authority_tx` and awaits the user's decision. `turn_interrupted` if denied outright, or proceeds on grant.
   - Sandbox setup for `SandboxTier::from(tool.effect_class())`, unprivileged sequence.
   - Tool execution inside the sandbox. Output artifact persisted. `effect_record` + `tool_result` events written.
   - `tool_tx` notified; TUI re-renders the nested block.
7. `TurnDriver` loops back to step 3 with updated messages (new `ToolResult` content blocks appended).
8. On `stop_reason: EndTurn`, validators run. On pass: `checkpoint` event written, `turn_committed`. On fail: overlay discard (Tier B rollback), `turn_aborted { reason: "validator_fail" }`.
9. On user `Ctrl+C` mid-turn: cancellation token flipped, adapter stream drops, in-flight tools are cooperatively cancelled via `ExecutionContext::cancelled`, `turn_interrupted { reason: "user_cancel" }` written.

## Error model

All provider errors normalize into a stable internal enum — the runtime never sees vendor-specific codes:

```rust
pub enum AdapterErrorCode {
    RateLimited,
    AuthFailed,
    InvalidRequest,
    ContextTooLong,
    ContentFilter,
    Timeout,
    Network,
    Unknown,
}

pub struct AdapterError {
    pub code: AdapterErrorCode,
    pub retryable: bool,
    pub provider_status: Option<u16>,
    pub detail: String,
}
```

Tool errors become `is_error: true` `ToolResult` blocks — never panics, never bubble out of `TurnDriver`. Runtime errors (disk, schema corruption, cgroup setup failure) surface as `turn_aborted { reason: "runtime_error" }` and a red status-line message; the session continues if the corrupt turn was partial.

## Resume and session lifecycle (LOW-1)

On `azoth` launch or `/resume <run_id>`:

1. Session loader opens `<run_id>.jsonl`, scans for dangling turns, appends synthetic `turn_interrupted { reason: "crash" }` markers where needed.
2. Forensic projection loads into TUI scrollback — user sees the full history including interrupted turns (grayed out, marked non-replayable).
3. Replayable projection rebuilds the Context Kernel's durable state: last committed checkpoint, active contract, capability tokens, token usage.
4. TUI shows a banner: `resumed run_abc123 · contract ctr_... · last checkpoint chk_... (7 turns, 2 interrupted)`.
5. The first user input of the resumed session is preceded by a *contract validity check* prompt: `contract still valid? [y/amend/abandon]`. No new turn starts until this is answered.

This guarantees that invariant 1 ("transcript is not memory") is not accidentally violated by a well-meaning resume path that replays raw messages.

## Verification (how to know the architecture holds)

1. **`cargo check --workspace`** passes with zero feature flags and zero warnings on Linux x86_64.
2. **`cargo test -p azoth-core`** covers:
   - Serde round-trip for every schema type.
   - Content-block translation both directions: `ModelTurnRequest`/`ModelTurnResponse` → OpenAI Chat Completions wire shape → back, and → Anthropic Messages wire shape → back. Parallel tool calls with `call_group_id` preserve ordering.
   - `EffectClass → SandboxTier` mapping is exhaustive (compile test).
   - Negative `Tainted<T>` compile tests: a construct that tries to pass `Tainted<Value>` where typed tool input is expected must fail to compile (`trybuild` fixture).
   - `SecretHandle` has no `Serialize` impl; Debug prints `[REDACTED]`.
   - JSONL dual projection: given a fixture file with one committed turn, one interrupted turn, and one dangling turn, the replayable projection yields exactly the committed turn; the forensic projection yields all three with correct annotations.
3. **Migration integration test** (LOW-2): `cargo test` runs every `refinery` migration in order against an in-memory SQLite, asserts schema validity and that FTS5 content tables exist before their virtual tables.
4. **Headless adapter fixture test**: drive one turn through `AnthropicMessagesAdapter` pointed at a recorded HTTP fixture (no network). Assert the turn JSONL is well-formed and a checkpoint is written.
5. **Manual TUI smoke, minimal**: launch TUI against a mock adapter that echoes user input, type `hello`, observe streaming render, `/quit`. Session file is well-formed on inspection.
6. **Manual TUI smoke, tool round-trip**: mock adapter issues a synthetic `tool_use` for a test tool that returns a fixed payload. Approval modal appears, user grants once, tool executes, result renders nested under the tool_use block, turn commits.
7. **Sandbox smoke**: Tier A child cannot write to `/etc/passwd` (EACCES or EPERM surfaced via `ToolError::SandboxDenied`). Tier B child can write inside the fuse-overlayfs merged dir but the repo snapshot stays pristine after the child exits.
8. **Backpressure smoke** (MED-3): mock adapter emits 10,000 `TextDelta`s as fast as possible; `Ctrl+C` during the stream registers in under 100 ms and writes `turn_interrupted { reason: "user_cancel" }`.

## What v1 does NOT ship (explicit scope fence)

- Graph / symbol / LSP retrieval — `GraphRetrieval` trait stub only.
- Policy DSL — hardcoded v1 policy only.
- Contract amendments — lint + accept only.
- `azoth replay`, `azoth export` — events recorded, commands deferred.
- Trace graders / eval flywheel.
- gVisor (Tier C) and Firecracker (Tier D).
- Contract-diff review UI, evidence-pack editor.
- Domain packs — coding only.
- Episodic memory beyond checkpoints.
- Provider routing / fallback policy — first configured profile wins.
- Team/enterprise deployment modes.
- Red-team suite.

Each of these is addressable in a later plan without touching the schemas, event model, crate layout, or trait shapes defined here.

## Critical files (once implementation begins)

- `crates/azoth-core/src/schemas/mod.rs` — the type hub; every other module depends on it.
- `crates/azoth-core/src/event_store/jsonl.rs` — dual projection logic (CRIT-1 lives here).
- `crates/azoth-core/src/authority/tainted.rs` — `Tainted<T>` + `Extractor` + `SecretHandle`.
- `crates/azoth-core/src/execution/dispatcher.rs` — `ErasedTool` blanket impl (CRIT-2 lives here).
- `crates/azoth-core/src/sandbox/tier_a.rs`, `tier_b.rs` — unprivileged syscall sequence (CRIT-3 lives here).
- `crates/azoth-core/src/adapter/mod.rs` — `ProviderAdapter` trait, `ProviderProfile`, `TokenizerFamily`.
- `crates/azoth-core/src/adapter/anthropic_messages.rs` — native shape adapter.
- `crates/azoth-core/src/adapter/openai_chat_completions.rs` — downcast adapter, OpenRouter strict header, parallel-tool `call_group_id` handling (HIGH-3 lives here).
- `crates/azoth-core/src/context/kernel.rs` — 5-lane packet compiler; local tokenizer dispatch (MED-1 lives here).
- `crates/azoth-core/src/turn/driver.rs` — the `TurnDriver` state machine.
- `crates/azoth/src/tui/app.rs` — `biased` `tokio::select!` loop, bounded channels (MED-3 lives here).
- `crates/azoth/src/tui/render.rs` + `widgets/` — ratatui frame builder.

## Critical external dependencies

| Crate | Version | Purpose | Risk |
|---|---|---|---|
| `ratatui` | 0.30 | TUI framework | stable, pre-1.0 API but production-used |
| `crossterm` | 0.29 (feature `event-stream`) | terminal backend + async events | stable |
| `tui-textarea` | 0.7 | multi-line input widget | community-maintained, de facto standard |
| `tokio` | 1.x | async runtime | stable |
| `rusqlite` | 0.39 | SQLite driver, FTS5 via raw SQL | stable |
| `refinery` | 0.9 | SQLite migrations | stable |
| `landlock` | 0.4 | FS ambient-rights sandbox | lags kernel ABI; v1-level functionality only |
| `seccompiler` | 0.5 | syscall filter builder | rust-vmm, stable |
| `nix` / `rustix` | latest | low-level syscalls (unshare, mount, prctl) | stable |
| `serde` + `serde_json` | 1.x | serialization | stable |
| `async-trait` | 0.1 | trait objects with async fn | stable |
| `clap` | 4.x | CLI arg parsing | stable |
| `tiktoken-rs` | latest | OpenAI tokenizer for packing decisions | stable |
| `reqwest` | latest (feature `stream`) | HTTP client for adapters | stable |
| `tracing` | 0.1 | structured logging | stable |
| **runtime**: `fuse-overlayfs` | system binary | unprivileged overlayfs for Tier B | external binary, probed at startup |

## Next actions after plan approval

1. Scaffold the two-crate workspace (`cargo new --lib crates/azoth-core`, `cargo new crates/azoth`).
2. Land `azoth-core` schemas + JSONL dual projection + `Tainted<T>` + `SecretHandle` — all the type-level foundations — in a single focused PR. No adapters, no TUI, no sandbox yet. This is the smallest shippable unit that proves the architecture compiles.
3. Subsequent PRs each land one subsystem end-to-end: `AnthropicMessagesAdapter` + fixture test → `ToolDispatcher` + a single `repo.search` tool → `ContextKernel` v0 → TUI skeleton → Tier A sandbox → `TurnDriver` integrating them all.
4. Each of those PRs gets its own sprint-level plan (separate `/plans/` entries). This architecture spec is the invariant anchor they all reference.
