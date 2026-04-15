I’ll answer as a principal architect for secure coding agents and long-running tool-using runtimes, the person who has to turn a seductive architecture memo into something juniors can actually build without setting the repo, the budget, or the user’s trust on fire.

Two honest guardrails before the blueprint.

First, **no one can truthfully validate this as “perfect, without mistakes.”** OpenAI’s own safety guidance says agents can still make mistakes or be tricked, and Anthropic’s eval guidance says teams need rigorous evals precisely because intuition is not enough once agents become complex. So I’m not going to lie to you in a polished tone and call that rigor. ([OpenAI Developers][1])

Second, I’m going to audit the **load-bearing lines** of the last message, not every decorative sentence. That is what matters for a PRD and implementation plan. Humans waste whole quarters polishing adjectives around the wrong abstractions.

# Part I. Audit of the previous blueprint, line by line by importance

## 1. “Azoth should be contract-centric”

**Verdict: keep, strengthen, make mandatory.**

This is correct. OpenAI’s current guidance for long-running and coding-heavy tasks says the biggest gains come when prompts specify the **output contract, tool-use expectations, and completion criteria**, and Anthropic’s eval guidance says good evals need explicit grading logic rather than vibes. METR’s March 2026 result is the ugly proof: roughly half of test-passing SWE-bench Verified PRs from recent agents still would not be merged by maintainers, so “tests passed” is not enough and “model says done” is definitely not enough. ([OpenAI Developers][2])

**Correction:** the contract must not be a nice optional intro screen. It must be a first-class runtime object with validation rules, versioning, amendments, and explicit acceptance by a human or by trusted automation. Otherwise it turns back into a prompt paragraph wearing a fake mustache.

**User benefit:** Azoth stops freelancing the definition of success. That alone is worth a lot.

## 2. “Azoth should be context-compiled, not transcript-driven”

**Verdict: keep, make it the center of the product.**

This is the strongest architectural idea in the whole thread. Anthropic’s context-engineering writeup says context is a **critical but finite resource**, and long-running agents need active curation of what enters the model. OpenAI’s stateful Responses/Agents docs similarly treat continuity as managed state, not “just append more chat,” and “Lost in the Middle” shows long-context models still use long inputs unevenly, with relevant information in the middle often used worse than information near the beginning or end. ([Anthropic][3])

**Correction:** the previous message was right in spirit but not explicit enough in implementation. The context compiler must be its **own subsystem** with inputs, outputs, metrics, and failure modes. It is not a helper function.

**User benefit:** the agent stays coherent across long runs without drowning the model in stale garbage. That is what people actually feel in practice.

## 3. “Azoth should be event-sourced”

**Verdict: keep, but tighten the meaning.**

This is well supported. OpenAI’s trace-grading guidance defines the trace as the end-to-end log of decisions, tool calls, and reasoning steps, and recommends grading traces to find where orchestration or behavior breaks. Anthropic’s long-running harness work also points toward persistent artifacts and explicit bridging between sessions, not ephemeral chat magic. ([OpenAI Developers][4])

**Correction:** event sourcing does **not** equal replayability. Replay must be tracked per event class, because some actions are deterministic, some are bounded-drift, and some are observational only. The previous message got close to this but needs a stricter runtime contract.

**User benefit:** debugging stops being folklore. You can answer “what happened?” and “why?” without inventing mythology from a transcript.

## 4. “Azoth should be provider-agnostic”

**Verdict: keep, but only at the runtime boundary.**

This is the right strategic choice. Anthropic’s “Building effective agents” explicitly favors simple, composable systems over framework dependency, and OpenAI’s docs show real differences in state management, WebSocket execution, caching, and structured outputs that justify thin provider-specific adapters over one fake-universal abstraction. The provider-agnostic layer should be **your internal protocol**, not the model request payload itself. ([OpenAI Developers][5])

**Correction:** do not pretend providers are interchangeable at runtime semantics level. They are not. OpenAI, Anthropic, and Google differ in state handling, tool semantics, compaction, and structured output behavior. Azoth should abstract over them internally while still shipping **provider profiles** tuned to published capabilities. ([OpenAI Developers][5])

**User benefit:** no framework lock-in, but also no delusion that one generic wrapper gives you the best behavior everywhere.

## 5. “Azoth should be coding-first but ready for any task”

**Verdict: keep, with a clear split between domain kernel and domain adapters.**

This is solid. Coding remains the best proving ground because it has deterministic validators, rollback, diffs, tests, and measurable regressions. Anthropic’s and OpenAI’s 2025-2026 agent guidance both focus heavily on tool use, long-running workflows, and coding harnesses because they are easier to ground and evaluate. ([OpenAI Developers][2])

**Correction:** do not claim “any task” in the abstract. The runtime should be task-general, but only through **domain packs** that specify contracts, validators, tools, and evidence formats. Otherwise “task generality” becomes a polite way to say “we didn’t define anything.”

**User benefit:** one runtime, many domains, without turning the core into mush.

## 6. “Azoth should rely on repo-native knowledge”

**Verdict: keep, strongly.**

This is one of the most evidence-backed parts. RANGER shows repository-level graph-enhanced retrieval outperforming retrieval baselines across multiple automated software engineering tasks, and TDAD shows AST/code-test graph impact analysis can reduce regressions sharply while improving resolution. Anthropic’s contextual retrieval work also supports targeted retrieval over stuffing history into the prompt. ([arXiv][6])

**Correction:** repo intelligence must be **lazy and incremental** by default for hardware sanity. Build just enough graph/index state for the touched repo areas, then deepen on demand.

**User benefit:** far better code localization, less random file wandering, fewer regressions.

## 7. “Azoth should treat approvals and sandboxing as separate controls”

**Verdict: keep, non-negotiable.**

OpenAI’s Codex docs say this explicitly: sandboxing and approvals are different controls that work together. Anthropic’s auto-mode post makes the same practical point from the other side: people approve too many prompts, but zero-approval autonomy is also unsafe. They report users approve 93% of permission prompts, and they cite incidents from overeager behavior. ([OpenAI Developers][7])

**Correction:** Azoth must support **standing scoped capability grants**, not just manual prompts or “YOLO mode.” Permission spam is not safety. It is learned helplessness with buttons.

**User benefit:** high autonomy without approval fatigue or stupidly broad trust.

## 8. “Azoth should have an evaluation plane”

**Verdict: keep, mandatory from day one.**

This is correct. Anthropic’s eval post says good evaluations make behavioral changes visible before they hit users, and OpenAI’s trace-grading docs say traces should be scored systematically to find regressions and validate changes. Anthropic’s engineering page also notes infra configuration can swing coding benchmark scores by several points, sometimes more than the model gap. ([Anthropic][8])

**Correction:** evals are not just offline benchmarks. Azoth needs **runtime telemetry-grade evals**, **regression suites**, and **post-deploy operational metrics**.

**User benefit:** the team can tell whether Azoth is actually getting better, not just louder.

---

# Part II. The build-grade blueprint for `Azoth`

This is the version I would let juniors and mid-level engineers build against.

## 1. Product definition

`Azoth` is a **local-first, provider-agnostic CLI agent runtime** for long-running, tool-using work. Its first-class domain is software engineering, but its core runtime is domain-general through adapters. It is not a chatbot. It is not a workflow canvas. It is not an “AI framework.” It is a **governed operator runtime** that uses models to plan and explain, while deterministic policy, validators, and approvals control what is allowed to happen. This direction is directly aligned with OpenAI’s stateful agent guidance, Anthropic’s emphasis on harness/context design, and recent evidence that raw transcript-based agents degrade badly over long runs. ([OpenAI Developers][5])

### Primary promise

Azoth should help a user delegate meaningful work without losing:

* control,
* reproducibility,
* safety,
* or clarity about what the agent actually did. ([OpenAI Developers][1])

### Primary anti-promise

Azoth must **not** promise:

* perfect autonomy,
* perfect replay,
* or model infallibility.

Those claims are not supported by current provider safety guidance or recent benchmarking. ([OpenAI Developers][1])

## 2. Users and jobs to be done

### Core user segments

**Solo developer / power user**
Needs a CLI that can investigate, patch, test, and explain changes without constant reprompting. The core benefits are continuity, lower prompt maintenance, and safe autonomy. This is directly supported by Anthropic’s long-running harness framing and OpenAI’s stateful context tooling. ([Anthropic][9])

**Team engineer / reviewer**
Needs inspectable traces, clear diffs, approval scopes, and replay/evidence artifacts. That maps directly onto OpenAI trace grading and the METR finding that “test-passing” does not guarantee mergeability. ([OpenAI Developers][4])

**Ops / platform / security owner**
Needs hard boundaries, controlled side effects, auditable permissions, and the ability to disable or scope risky effects. OpenAI’s sandboxing and safety docs plus Anthropic’s auto-mode/security work make this a first-order design requirement, not a “later” feature. ([OpenAI Developers][7])

### Jobs Azoth must do on day one

For coding:

* investigate a bug,
* localize the likely cause,
* propose a bounded contract,
* edit in a reversible workspace,
* run targeted validation,
* present evidence,
* and either apply or escalate. ([Anthropic][9])

For non-coding:

* investigate,
* gather evidence,
* produce a structured conclusion or next-step plan,
* and preserve artifacts for continuation. ([Anthropic][9])

## 3. Non-goals for v1

Azoth v1 should **not** try to be:

* a voice assistant,
* a full GUI platform,
* a workflow marketplace,
* a multi-agent society,
* or a self-training/autonomous fine-tuning system.

Anthropic’s guidance strongly favors simple, composable systems over complicated agent stacks, and General AgentBench is a warning against assuming more parallelism or more interaction automatically yields better behavior. ([Anthropic][3])

This matters for juniors because “leave it for later” is not cowardice here. It is architecture hygiene.

## 4. The six invariants of Azoth

These are runtime laws.

### Invariant 1: Transcript is not memory

Raw chat history is only one evidence source. Durable state lives outside the model window. This is the direct consequence of Anthropic’s context-engineering guidance, OpenAI’s stateful chaining, and long-context limitations shown in “Lost in the Middle.” ([Anthropic][3])

### Invariant 2: Deterministic controls outrank model output

Policy engine, validators, capability grants, and approvals have final authority. The model cannot override legality. This follows from OpenAI’s safety guidance around prompt injections, policy examples, and careful MCP/tool use. ([OpenAI Developers][1])

### Invariant 3: Every non-trivial run has a contract

No bounded work without explicit success criteria, scope, and side-effect budget. This is supported by OpenAI’s emphasis on explicit output contracts and completion criteria for long-running tasks. ([OpenAI Developers][2])

### Invariant 4: Every side effect has a class

Nothing is “just a tool call.” Observe, stage, apply-local, apply-repo, apply-remote-reversible, apply-remote-stateful, and apply-irreversible are different classes with different controls. This is the logical extension of OpenAI’s explicit separation between sandboxing and approval policies. ([OpenAI Developers][7])

### Invariant 5: Every run leaves structured evidence

Trace, checkpoint, artifacts, approvals, validators, and replay class must be persisted. This is required for trace grading and long-running continuation. ([OpenAI Developers][4])

### Invariant 6: Every subsystem is eval-able

Context compiler, retrieval, tool routing, approvals, validators, and contract satisfaction must all be measurable. Anthropic and OpenAI both make eval discipline a central engineering requirement. ([Anthropic][8])

## 5. The top-level architecture

Azoth should have seven runtime subsystems.

### A. Contract Engine

Responsible for contract drafting, validation, versioning, amendment workflow, and satisfaction checks. Grounded by OpenAI’s guidance on explicit completion criteria and by the mergeability gap documented by METR. ([OpenAI Developers][2])

### B. Context Kernel

Responsible for compiling step-specific context packets from durable state and current evidence. Grounded by Anthropic context engineering and OpenAI state/compaction support. ([Anthropic][3])

### C. Knowledge Plane

Responsible for symbol/index/graph retrieval, episodic state, and evidence artifacts. Grounded by RANGER, TDAD, and contextual retrieval. ([arXiv][6])

### D. Policy and Routing Plane

Responsible for model invocation, provider adapters, reasoning-effort routing, and optional reviewer/risk scoring. Grounded by provider docs that show differing state, tool, and reasoning controls. ([OpenAI Developers][10])

### E. Effect and Authority Engine

Responsible for legality checks, taint handling, capability grants, side-effect classification, and approval routing. Grounded by OpenAI and Anthropic security/permission docs. ([OpenAI Developers][1])

### F. Execution Plane

Responsible for running tools inside the right sandbox tier and collecting structured outputs. Grounded by sandbox/approval docs and the need for isolation that is proportional to risk. ([OpenAI Developers][11])

### G. Evaluation Plane

Responsible for trace grading, regression testing, replay fidelity, contract fidelity, and operational dashboards. Grounded by OpenAI trace grading and Anthropic eval guidance. ([OpenAI Developers][4])

## 6. The core data model

This section is intentionally detailed because juniors otherwise invent ten slightly different JSON blobs and call it progress.

### 6.1 Contract

A `Contract` object must contain:

* `contract_id`
* `contract_type` = `execution` or `investigation`
* `intent_statement`
* `success_criteria[]`
* `failure_criteria[]`
* `in_scope_surfaces[]`
* `forbidden_surfaces[]`
* `effect_budget`
* `regression_envelope`
* `approval_policy`
* `signoff_policy`
* `author`
* `accepted_by`
* `version`
* `status` = `draft|accepted|amended|completed|failed|abandoned`

**Why this exact set:** OpenAI explicitly recommends giving long-running and coding-oriented prompts precise completion criteria and explicit tool-use expectations, while Anthropic’s eval guidance stresses explicit grading logic. ([OpenAI Developers][2])

### 6.2 Checkpoint

A `Checkpoint` object must contain:

* `checkpoint_id`
* `run_id`
* `parent_checkpoint_id`
* `touched_surfaces[]`
* `claims_established[]`
* `evidence_refs[]`
* `open_loops[]`
* `blocked_by[]`
* `pending_approvals[]`
* `validator_state`
* `next_safe_entrypoint`
* `summary_for_humans`
* `summary_for_model`

**Why:** Anthropic’s long-running harnesses rely on leaving clear artifacts across sessions; compaction alone is not enough. ([Anthropic][9])

### 6.3 Effect record

An `EffectRecord` must contain:

* `effect_id`
* `effect_class`
* `capability_required`
* `sandbox_tier`
* `rollback_protocol`
* `replay_class`
* `requested_by_step`
* `approved_by`
* `approved_scope`
* `started_at`
* `finished_at`
* `result_ref`

**Why:** approvals and sandboxing are separate controls, and side effects need different treatment by class. ([OpenAI Developers][7])

### 6.4 Context packet

A `ContextPacket` must contain exactly five lanes:

* `constitution_lane`
* `working_set_lane`
* `evidence_lane`
* `checkpoint_lane`
* `exit_criteria_lane`

It must also record:

* `token_budget`
* `candidate_items_considered`
* `items_selected`
* `items_dropped`
* `compiler_version`

**Why:** context engineering is iterative curation under finite budget, not freeform prompt stuffing. ([Anthropic][3])

### 6.5 Trace event

A `TraceEvent` must contain:

* `event_id`
* `run_id`
* `step_id`
* `timestamp`
* `event_type`
* `contract_id`
* `checkpoint_id_before`
* `checkpoint_id_after`
* `context_packet_id`
* `tool_call_ref`
* `validator_refs[]`
* `approval_ref`
* `replay_class`
* `artifact_refs[]`

**Why:** this is what makes trace grading and postmortem analysis possible. ([OpenAI Developers][4])

## 7. Contract Engine in detail

### 7.1 Contract types

**Execution contract**
Use when the desired end state is concrete and mechanically testable: fix bug, refactor module, add endpoint, update dependency, create report in fixed schema. This aligns with OpenAI’s guidance that performance improves when completion criteria are explicit. ([OpenAI Developers][2])

**Investigation contract**
Use when the end state is evidence, not immediate modification: isolate root cause, identify candidate files, determine whether a regression exists, produce ranked hypotheses. This avoids the human mistake of pretending exploratory work is already spec-complete. Anthropic’s long-running harness work is explicit that agents often need staged progress over sessions rather than one-shot completion. ([Anthropic][9])

### 7.2 Contract linting

Before acceptance, Azoth must lint the contract.

Mandatory lint rules:

* at least one success criterion is machine-checkable,
* forbidden surfaces are present for execution contracts,
* effect budget is present,
* regression envelope is present,
* sign-off policy is present,
* contract type matches task shape,
* scope is not empty,
* dangerous effect classes are not silently pre-authorized.

This is design reasoning, but it is directly motivated by OpenAI’s safety guidance on ambiguous input and harmful actions, and by the need for explicit grading logic in Anthropic’s eval guidance. ([OpenAI Developers][1])

### 7.3 Contract amendments

Amendments are first-class events. They require:

* delta from previous version,
* reason,
* new scope/effect implications,
* approval by user or trusted automation.

No silent scope expansion. That is exactly the kind of overeager initiative Anthropic cites in real incidents. ([Anthropic][12])

## 8. Context Kernel in detail

This is the heart of Azoth.

### 8.1 Inputs to the context compiler

For every non-trivial step, the compiler reads:

* active contract,
* current checkpoint,
* last validator results,
* unresolved blockers,
* touched surfaces,
* latest tool outputs,
* requested action type,
* model profile constraints.

This is supported by Anthropic’s framing of context as the curated set of tokens most likely to produce desired behavior. ([Anthropic][3])

### 8.2 Candidate evidence sources

The compiler may pull from:

* repo graph,
* exact text search,
* full-text search,
* tool logs,
* current diffs,
* prior checkpoints,
* issue/PR metadata,
* external retrieved docs,
* user notes and policies.

### 8.3 Selection policy

The compiler should prefer, in order:

1. exact match / lexical retrieval,
2. graph-neighbor retrieval,
3. current-task episodic evidence,
4. reranked ambiguous candidates.

This order is grounded by RANGER and contextual retrieval: targeted structured retrieval beats blind history stuffing. ([arXiv][6])

### 8.4 Packet packing rules

* Constitution goes first.
* One active goal only.
* One current hypothesis only.
* The most decision-critical evidence goes early, not buried in the middle.
* Long raw logs must be transformed into structured summaries plus artifact references.
* Old irrelevant turn history is never copied by default.

That packing policy is the practical response to “Lost in the Middle.” ([arXiv][13])

### 8.5 Context compiler metrics

Track:

* selected token count,
* dropped token count,
* retrieval hit rate,
* evidence reuse rate,
* downstream validator pass rate by packet shape,
* correction rate after packet mis-selection.

Because if the compiler is not measured, it will quietly become a prompt-shaped landfill. Anthropic and OpenAI both push evaluation discipline over intuition. ([Anthropic][8])

## 9. Knowledge Plane in detail

### 9.1 Semantic memory

For coding repos, maintain:

* file metadata,
* symbol table,
* cross-file references,
* dependency edges,
* code-test edges,
* full-text index,
* lexical index.

RANGER and TDAD both support graph-aware retrieval and impact analysis as high-value investments. ([arXiv][6])

### 9.2 Temporal memory

Store:

* commit history,
* co-edit frequency,
* prior failed attempts,
* review outcomes,
* incident tags.

This helps Azoth localize likely blast radius and reuse prior lessons. It is a principled extension of repo-native retrieval and checkpointing. ([arXiv][6])

### 9.3 Episodic memory

Store:

* checkpoints,
* contract amendments,
* decision outcomes,
* blocked paths,
* successful recipes tied to contract types.

This is what lets Azoth resume sanely after time or context boundaries. ([Anthropic][9])

### 9.4 Evidentiary memory

Store:

* diffs,
* test reports,
* trace artifacts,
* screenshots,
* logs,
* model outputs,
* approval evidence packs.

This is essential for replay classes, audits, and reviews. ([OpenAI Developers][4])

## 10. Effect and Authority Engine in detail

### 10.1 Effect classes

Use these classes exactly:

* `observe`
* `stage`
* `apply_local`
* `apply_repo`
* `apply_remote_reversible`
* `apply_remote_stateful`
* `apply_irreversible`

Do not let developers invent synonyms. Naming drift is how rules die.

### 10.2 Capability grants

Each effect must require a capability token with:

* scope,
* duration,
* surfaces,
* allowed tool namespaces,
* approved effect classes,
* revocation rules.

This is the right response to approval fatigue. Anthropic’s auto-mode post shows prompt-by-prompt approval scales badly, but unrestricted autonomy is unsafe. ([Anthropic][12])

### 10.3 Taint handling

All external or user-provided content is tainted:

* user free text,
* web results,
* repo content from untrusted repos,
* connector outputs,
* shell stdout/stderr,
* tool outputs.

Tainted content may be read by the model, but privileged tool arguments must be produced through deterministic extraction/validation before execution. This is directly supported by OpenAI’s prompt-injection safety guidance. ([OpenAI Developers][1])

### 10.4 Approval policy

Azoth must support:

* `manual_every_time`
* `scoped_once`
* `session_scope`
* `policy_auto`
* `forbidden`

Default policy:

* `observe` and low-risk `stage` inside scope: auto
* `apply_local`: scoped-once or session-scope
* `apply_repo` and above: manual or policy-auto depending on environment
* `apply_irreversible`: always manual

This matches the practical lessons in OpenAI Codex docs and Anthropic’s auto-mode/security posts. ([OpenAI Developers][7])

## 11. Execution Plane in detail

### 11.1 Sandbox tiers

Use proportional isolation.

**Tier 0: observe**
For read-only search, indexing, and graph queries. Lowest overhead.

**Tier 1: stage-local**
For overlay edits, formatting, builds, local tests.

**Tier 2: stronger isolated execution**
For code you do not fully trust, broader shell actions, or network-enabled staging work.

**Tier 3: high-risk remote/stateful execution**
For operations that can damage external systems.

This proportional model follows the logic in OpenAI’s sandboxing docs and avoids the earlier hardware mistake of turning every grep into a bunker operation. ([OpenAI Developers][7])

### 11.2 Tool outputs

Every tool output must be:

* structured,
* bounded,
* token-efficient,
* and artifact-linked for large payloads.

Anthropic explicitly recommends meaningful context, token-efficient tool responses, and careful tool descriptions/specs. ([Anthropic][14])

### 11.3 Minimal tool surface

For v1, only ship:

* `repo.search`
* `repo.read_spans`
* `repo.graph_query`
* `repo.diff_apply`
* `exec.run`
* `exec.tests`
* `exec.lsp`
* `vcs.local`
* `vcs.remote`
* `approval.request`
* `artifact.read`
* `artifact.write`

Anthropic’s tool-writing guidance warns that more tools do not automatically help, and namespacing is important. ([Anthropic][14])

## 12. Provider/model layer

Azoth must be provider-agnostic internally and provider-aware externally.

### 12.1 Internal abstraction

Define one internal `ModelTurnRequest` and `ModelTurnResponse` schema that includes:

* context packet,
* tool schema set,
* desired response schema,
* reasoning policy,
* max effort,
* citation/grounding requirements,
* continuity ids,
* phase.

This keeps the runtime independent of provider quirks.

### 12.2 OpenAI profile

Use OpenAI when you need:

* strong stateful chaining,
* `previous_response_id`,
* prompt caching,
* strict structured outputs,
* WebSocket mode for long tool-heavy rollouts.

OpenAI reports that Responses statefulness improves cache utilization compared with older chat flows, and WebSocket mode can improve end-to-end latency for 20+ tool-call rollouts by up to roughly 40%. Structured outputs guarantee JSON Schema adherence. ([OpenAI Developers][5])

### 12.3 Anthropic profile

Use Anthropic when you need:

* strong long-running harness behavior,
* context compaction,
* multi-session continuity patterns,
* strong coding/agentic behavior in large codebases,
* large context windows for selected models.

Anthropic’s engineering posts on context engineering and long-running harnesses make this profile especially strong for sustained work, and Opus 4.6 is publicly positioned for professional software engineering and complex agentic workflows. ([Anthropic][3])

### 12.4 Google profile

Use Google when you need:

* very large multimodal or repo-scale context handling,
* strong reasoning/coding on large datasets or code repositories,
* official function calling,
* structured outputs,
* and, where appropriate, the newer Interactions API for unified state/tool handling.

Google’s official docs position Gemini 2.5 Pro as a high-capability reasoning model that can comprehend even entire code repositories, and both function calling and structured outputs are documented capabilities. ([Google Cloud Documentation][15])

### 12.5 Routing policy

Do not route by mythology about hidden training. Route by **published capability surfaces**:

* state handling,
* tool semantics,
* structured output reliability,
* context size,
* latency,
* price,
* and coding/agentic positioning in public docs/system cards. ([OpenAI Developers][10])

## 13. Validation layer

### 13.1 Deterministic validators

For coding:

* syntax/build pass,
* unit tests,
* impact-selected regression tests,
* lint/formatter,
* static analysis,
* contract-specific assertions.

TDAD strongly supports test-impact analysis as a way to cut regressions and improve practical performance. ([arXiv][16])

For non-coding, swap in:

* schema checks,
* reconciliation rules,
* policy checks,
* evidence completeness rules,
* output-contract conformance.

### 13.2 Reviewer model

A reviewer/risk model is optional but useful on non-trivial actions. It may:

* flag risk,
* summarize blind spots,
* recommend escalation.

It does **not** approve illegal actions or define success. That mistake has already died several times in this thread. Let it stay dead.

## 14. Evaluation plane in detail

### 14.1 Metrics

Track at least:

* contract fidelity,
* scope discipline,
* amendment rate,
* approval quality,
* regression escape rate,
* context compiler success rate,
* replay fidelity by class,
* human sign-off agreement,
* post-merge survival,
* time-to-safe-completion.

This follows directly from OpenAI trace grading and Anthropic’s eval guidance. ([OpenAI Developers][4])

### 14.2 Eval categories

Use three kinds:

* deterministic code/data graders,
* model-based trace graders,
* human review on sampled high-risk runs.

Anthropic explicitly recommends combining grading techniques to match system complexity. ([Anthropic][8])

### 14.3 Benchmark discipline

Every benchmark run must log:

* model version,
* provider,
* prompts/contracts version,
* hardware/infrastructure setup,
* network settings,
* timeout policies,
* sandbox tier.

Anthropic’s engineering page notes that infra configuration can swing agentic coding scores by several points, sometimes more than the leaderboard gap. ([Anthropic][17])

## 15. Hardware-aware deployment modes

Because the earlier hardware criticism was fair.

### Lite mode

Use:

* local CLI,
* one provider,
* no always-on semantic services,
* no high-risk remote effects,
* no heavyweight isolation by default.

Best for laptops and solo users.

### Standard mode

Add:

* incremental repo graph,
* staged validation,
* optional stronger isolation,
* approval policy automation.

Best for team development.

### Secure mode

Add:

* stronger isolated execution for risky effects,
* central event shipping,
* policy-managed capabilities,
* replay/reporting for audits.

Best for enterprises.

This avoids repeating the stupid mistake of making the secure maximum the default minimum.

## 16. Implementation roadmap for junior and mid-level engineers

### Phase 0. Architecture skeleton

Deliver:

* internal schemas,
* event log writer,
* contract objects,
* checkpoint objects,
* provider adapter interfaces,
* effect classes,
* capability grants,
* artifact store.

Acceptance:

* can create a run,
* can persist and reload it,
* can create a contract and one checkpoint,
* can record one tool call and one validator result.

### Phase 1. Minimal coding loop

Deliver:

* `repo.search`
* `repo.read_spans`
* `exec.run`
* `repo.diff_apply`
* `exec.tests`
* contract drafting for execution/investigation
* simple context compiler
* local approval flow

Acceptance:

* agent can investigate a bug, propose a contract, edit in stage, run tests, and present a diff plus evidence package.

### Phase 2. Repo intelligence

Deliver:

* symbol extraction,
* dependency edges,
* full-text index,
* impact-based test selection,
* episodic checkpoints,
* contextual retrieval ranking.

Acceptance:

* agent localizes likely files faster than lexical search alone on internal eval set,
* regression rate drops on chosen coding tasks.

### Phase 3. Security and authority

Deliver:

* taint engine,
* capability tokens,
* effect-class policy enforcement,
* richer approval scopes,
* stricter sandbox tier mapping.

Acceptance:

* prompt-injection test cases cannot directly drive privileged tool arguments,
* out-of-scope edits are blocked unless contract is amended.

### Phase 4. Provider profiles and routing

Deliver:

* OpenAI adapter,
* Anthropic adapter,
* Google adapter,
* provider profile configs,
* routing policy,
* fallback behavior.

Acceptance:

* same internal run can execute across providers with only adapter/config changes,
* contracts, traces, checkpoints, and effect records remain identical internally.

### Phase 5. Evaluation system

Deliver:

* trace grader,
* offline regression suite,
* runtime metrics,
* replay-class reports,
* human review queue.

Acceptance:

* every release candidate runs evals,
* regressions are visible by subsystem,
* benchmark results include infra metadata.

## 17. PRD seed requirements

These are the PRD-level must-haves.

Azoth v1 must:

* support execution and investigation contracts,
* compile context per step,
* keep transcript out of the critical path,
* classify all effects,
* enforce scoped approvals,
* support at least one coding repository end to end,
* persist trace/checkpoint/artifacts,
* and ship with an eval harness.

Azoth v1 must not:

* depend on any agent framework,
* require multi-agent orchestration,
* or require giant local hardware to function.

That last one is there because we already paid tuition for that mistake.

## 18. Final verdict

This is the first blueprint in the thread that is detailed enough to become a PRD foundation and disciplined enough that juniors can build it without inventing theology in the gaps.

The build-grade synthesis is this:

**Azoth is a contract-centric, context-compiled, event-sourced CLI runtime where deterministic policy and validators outrank model output, repo-native knowledge feeds a measured context compiler, side effects are classified and governed by explicit capabilities and approvals, provider adapters translate a stable internal protocol into vendor APIs, and every serious run produces enough evidence to evaluate, resume, audit, and improve the system over time.** ([OpenAI Developers][2])

That is not perfect.

It is something better than perfect-sounding.
It is buildable.

[1]: https://developers.openai.com/api/docs/guides/agent-builder-safety "Safety in building agents | OpenAI API"
[2]: https://developers.openai.com/api/docs/guides/prompt-guidance "Prompt guidance for GPT-5.4 | OpenAI API"
[3]: https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents?utm_source=chatgpt.com "Effective context engineering for AI agents"
[4]: https://developers.openai.com/api/docs/guides/trace-grading "Trace grading | OpenAI API"
[5]: https://developers.openai.com/api/docs/guides/migrate-to-responses?utm_source=chatgpt.com "Migrate to the Responses API"
[6]: https://arxiv.org/abs/2509.25257?utm_source=chatgpt.com "RANGER -- Repository-Level Agent for Graph-Enhanced Retrieval"
[7]: https://developers.openai.com/codex/concepts/sandboxing?utm_source=chatgpt.com "Sandbox – Codex"
[8]: https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents "Demystifying evals for AI agents \ Anthropic"
[9]: https://www.anthropic.com/engineering/effective-harnesses-for-long-running-agents?utm_source=chatgpt.com "Effective harnesses for long-running agents"
[10]: https://developers.openai.com/api/docs/guides/latest-model?utm_source=chatgpt.com "Using GPT-5.4 | OpenAI API"
[11]: https://developers.openai.com/codex/agent-approvals-security?utm_source=chatgpt.com "Agent approvals & security – Codex"
[12]: https://www.anthropic.com/engineering/claude-code-auto-mode?utm_source=chatgpt.com "Claude Code auto mode: a safer way to skip permissions"
[13]: https://arxiv.org/abs/2307.03172 "[2307.03172] Lost in the Middle: How Language Models Use Long Contexts"
[14]: https://www.anthropic.com/engineering/writing-tools-for-agents?utm_source=chatgpt.com "Writing effective tools for AI agents—using ..."
[15]: https://docs.cloud.google.com/vertex-ai/generative-ai/docs/models/gemini/2-5-pro?utm_source=chatgpt.com "Gemini 2.5 Pro | Generative AI on Vertex AI"
[16]: https://arxiv.org/abs/2603.17973?utm_source=chatgpt.com "TDAD: Test-Driven Agentic Development - Reducing Code Regressions in AI Coding Agents via Graph-Based Impact Analysis"
[17]: https://www.anthropic.com/engineering "Engineering \ Anthropic"
