# AZOTH — AI Agent Operating Manual

> This file is consumed exclusively by AI coding agents. No human-readable prose.
> Every statement is a directive or a verified fact. Act on them, do not summarize them.

---

## Identity

- **Codename**: AZOTH
- **Nature**: Claude Code CLI — the terminal-native AI coding agent by Anthropic
- **Runtime**: Bun (primary), Node.js (fallback). Check: `typeof Bun !== 'undefined'`
- **Language**: TypeScript (strict). React/Ink for terminal UI. Zod for schemas.
- **Module system**: ESM. All relative imports MUST use `.js` extension.
- **License**: Apache 2.0

---

## Repository Structure

```
azoth/
├── bridge/          # 32 files — IDE/desktop bridge, session management, JWT, remote execution
├── cli/             # 17 files — CLI transports (SSE, WS, CCR), MCP/plugin handlers, exit/update
│   ├── handlers/    # Auth, MCP, agents, plugins, autoMode, utility commands
│   └── transports/  # HybridTransport, WebSocketTransport, SSETransport, ccrClient, SerialBatchEventUploader
├── daemon/          # 14 files — Supervisor, worker pool, IPC protocol, permissions, conversation cache
├── buddy/           # 6 files — Companion sprite React component system
├── assistant/       # 1 file — Session history tracking
├── contrib/         # systemd + launchd service files for daemon
│   ├── systemd/     # claude-daemon.service (Type=notify, WatchdogSec=60s)
│   └── launchd/     # com.anthropic.claude-daemon.plist (KeepAlive, RunAtLoad)
├── assets/          # Static assets
├── CLAUDE.md        # THIS FILE — AI agent operating manual
├── README.md        # Human-facing documentation
└── LICENSE          # Apache 2.0
```

Full app (~512K lines, ~1900 files) is NOT in this repo. This is a partial source snapshot containing bridge/, cli/, daemon/, buddy/, assistant/. The core engine (coordinator/, tools/, context/, ink/, services/, commands/) is referenced by import paths like `src/services/...` or `../utils/...` but lives outside this tree.

---

## Architecture Layers

```
LAYER 0 — DAEMON (new)
  daemon/supervisor.ts    → Unix socket server, worker orchestration
  daemon/workerRegistry.ts → process pool (spawn, monitor, restart, park)
  daemon/authManager.ts   → OAuth lifecycle, in-memory token, broadcast
  daemon/permissionRouter.ts → TTL broker, multi-client broadcast, first-wins
  daemon/client.ts        → thin CLI: claude daemon start/stop/status/logs/approve/deny

LAYER 1 — BRIDGE (existing)
  bridge/bridgeMain.ts    → runBridgeLoop(), runBridgeHeadless() [daemon entry at L2810]
  bridge/sessionRunner.ts → child_process.spawn(), NDJSON relay, activity tracking
  bridge/bridgeApi.ts     → HTTP client for Environments API (register, poll, ack, stop, archive)
  bridge/replBridge.ts    → REPL-embedded bridge, BridgeCoreParams [daemon-callable at L91]

LAYER 2 — TRANSPORT
  cli/transports/HybridTransport.ts  → WS reads + HTTP POST writes (v1)
  cli/transports/SSETransport.ts     → Server-Sent Events (CCR v2)
  cli/transports/WebSocketTransport.ts → Pure WS fallback
  cli/transports/ccrClient.ts        → CCR v2 protocol client
  cli/transports/SerialBatchEventUploader.ts → Serialized POST queue with backoff

LAYER 3 — CLI SURFACE
  cli/structuredIO.ts     → SDK message I/O, permission hooks, tool result processing
  cli/print.ts            → Headless output, NDJSON serialization
  cli/handlers/mcp.tsx    → MCP subcommands (serve, add, remove, test, list)
  cli/handlers/auth.ts    → Authentication commands
```

---

## Critical Types — Know These

```typescript
// bridge/types.ts — the core interfaces
BridgeConfig         // dir, machineName, branch, gitRepoUrl, maxSessions, spawnMode, ...
SessionHandle        // sessionId, done: Promise, kill(), writeStdin(), updateAccessToken()
SessionSpawner       // spawn(opts, dir): SessionHandle
BridgeApiClient      // registerBridgeEnvironment, pollForWork, acknowledgeWork, stopWork, ...
BridgeLogger         // printBanner, logSessionStart, logStatus, logError, ...
SpawnMode            // 'single-session' | 'worktree' | 'same-dir'
WorkResponse         // API poll result with work.secret
PermissionResponseEvent // control_response with behavior

// bridge/bridgeMain.ts:2785
HeadlessBridgeOpts   // dir, spawnMode, capacity, sandbox, getAccessToken, onAuth401, log, onPermissionRequest
BridgeHeadlessPermanentError  // supervisor should NOT retry (exit code 78)
BackoffConfig        // connInitialMs, connCapMs, connGiveUpMs, generalInitialMs, ...

// daemon/ipcProtocol.ts — IPC frame types
DaemonRequest / DaemonResponse   // { id, type, payload }
WorkerIpcMessage                 // { type: WorkerIpcType, payload }
StartWorkerPayload               // { dir, spawnMode, capacity, sandbox, prewarm }
PermissionRequestPayload         // { requestId, workerKey, sessionId, toolName, ... }
PermissionResponsePayload        // { requestId, behavior: 'allow' | 'deny' }
WorkerInfo                       // { key, dir, pid, status, activeSessions, uptimeMs, ... }
WorkerStatus                     // 'starting' | 'running' | 'parked' | 'restarting' | 'stopping'
```

---

## Key Constants

```
SPAWN_SESSIONS_DEFAULT           = 32          // max sessions per worker
BRIDGE_POINTER_TTL_MS            = 14400000    // 4 hours
DEFAULT_SESSION_TIMEOUT_MS       = 86400000    // 24 hours
STATUS_UPDATE_INTERVAL_MS        = 1000
TOOL_DISPLAY_EXPIRY_MS           = 30000
SHIMMER_INTERVAL_MS              = 150
BATCH_FLUSH_INTERVAL_MS          = 100         // HybridTransport event buffering
POST_TIMEOUT_MS                  = 15000
CLOSE_GRACE_MS                   = 3000
MAX_RESOLVED_TOOL_USE_IDS        = 1000        // ring buffer dedup in structuredIO
MAX_WORKTREE_FANOUT              = 50
TOKEN_REFRESH_BUFFER_MS          = 300000      // 5 min before JWT expiry
EXIT_CODE_OK                     = 0
EXIT_CODE_TRANSIENT              = 1
EXIT_CODE_PERMANENT              = 78          // sysexits.h EX_CONFIG
```

---

## Feature Flags

Compile-time via `bun:bundle` `feature()`. Runtime polyfill at `node_modules/bundle/index.js`.

| Flag | Purpose |
|------|---------|
| `KAIROS` | Assistant / daily-log mode |
| `PROACTIVE` | Proactive autonomous mode |
| `BRIDGE_MODE` | VS Code / JetBrains IDE bridge |
| `VOICE_MODE` | Voice input via native audio |
| `COORDINATOR_MODE` | Multi-agent swarm coordinator |
| `TRANSCRIPT_CLASSIFIER` | Auto-mode permission classifier |
| `BASH_CLASSIFIER` | Bash command safety classifier |
| `BUDDY` | Companion sprite animation |
| `WEB_BROWSER_TOOL` | In-process web browser |
| `CHICAGO_MCP` | Computer Use (screen control) |
| `AGENT_TRIGGERS` | Scheduled cron agents |
| `ULTRAPLAN` | Ultra-detailed planning mode |
| `MONITOR_TOOL` | MCP server monitoring |
| `TEAMMEM` | Shared team memory |
| `EXTRACT_MEMORIES` | Background memory extraction |
| `MCP_SKILLS` | Skills from MCP servers |

GrowthBook gates (runtime):
- `tengu_ccr_bridge_multi_session` — multi-session spawn
- `tengu_ccr_bridge_multi_environment` — multi-environment per host
- `tengu_bridge_poll_interval_config` — dynamic poll tuning

---

## Import Conventions

```typescript
// ✅ CORRECT — always .js extension for relative imports
import { createSocketServer } from './socketServer.js'
import { runBridgeHeadless } from '../bridge/bridgeMain.js'

// ✅ CORRECT — node: prefix for builtins
import { createServer } from 'net'
import { spawn } from 'child_process'
import { readFile } from 'fs/promises'

// ✅ CORRECT — bare specifiers for npm packages
import axios from 'axios'
import { z } from 'zod/v4'
import React from 'react'

// ✅ CORRECT — src/ prefix for full-app modules (outside this tree)
import { getGlobalConfig } from '../utils/config.js'
import { Tool } from 'src/Tool.js'

// ❌ WRONG — never .ts extension in imports
import { foo } from './bar.ts'

// ❌ WRONG — never omit extension for relative paths
import { foo } from './bar'
```

---

## Code Patterns — Follow These

### 1. Dependency Injection (dominant pattern)

All core modules accept injected dependencies. Never import singletons directly in hot paths.

```typescript
// ✅ How bridge does it — inject everything
export function createSessionSpawner(deps: SessionSpawnerDeps): SessionSpawner { ... }
export function createBridgeApiClient(opts: BridgeApiClientOpts): BridgeApiClient { ... }
export async function runBridgeHeadless(opts: HeadlessBridgeOpts, signal: AbortSignal): Promise<void> { ... }

// ✅ How daemon does it — same pattern
export function createWorkerRegistry(opts: WorkerRegistryOpts): WorkerRegistry { ... }
export function createPermissionRouter(opts: PermissionRouterOpts): PermissionRouter { ... }
export function createAuthManager(): AuthManager { ... }
```

### 2. Schema Validation (Zod + lazySchema)

```typescript
// ✅ Pattern from bridge/bridgePointer.ts
import { z } from 'zod/v4'
import { lazySchema } from '../utils/lazySchema.js'

const MySchema = lazySchema(() =>
  z.object({
    field: z.string(),
    source: z.enum(['standalone', 'repl', 'daemon']),
  }),
)
export type MyType = z.infer<ReturnType<typeof MySchema>>
```

### 3. Error Classification (permanent vs transient)

```typescript
// ✅ Permanent error — supervisor parks the worker, no retry
throw new BridgeHeadlessPermanentError('Workspace not trusted')

// ✅ Transient error — supervisor retries with exponential backoff
throw new Error('Token unavailable, will retry')
```

### 4. Backoff Configuration

```typescript
// ✅ Reuse the exact BackoffConfig type from bridge/bridgeMain.ts:59
const DEFAULT_BACKOFF: BackoffConfig = {
  connInitialMs: 2_000,
  connCapMs: 120_000,
  connGiveUpMs: 600_000,
  generalInitialMs: 500,
  generalCapMs: 30_000,
  generalGiveUpMs: 600_000,
}
```

### 5. IPC Frame Protocol

```typescript
// ✅ 4-byte big-endian length prefix + UTF-8 JSON
// Used in: daemon/ipcProtocol.ts, matches sessionRunner.ts NDJSON pattern
import { frameEncode, FrameDecoder } from './ipcProtocol.js'

// Encode
socket.write(frameEncode({ type: 'ping' }))

// Decode (stateful, handles partial reads)
const decoder = new FrameDecoder((msg) => handleMessage(msg))
socket.on('data', (data) => decoder.push(data))
```

### 6. Ring Buffer for Bounded Collections

```typescript
// ✅ Fixed-size, oldest-overwritten, used for logs and activity tracking
import { RingBuffer } from './ringBuffer.js'
const logs = new RingBuffer<string>(1000)
logs.push('new entry')        // O(1), overwrites oldest if full
const all = logs.toArray()    // oldest-first
```

### 7. Graceful Shutdown via AbortController

```typescript
// ✅ Pattern used by bridge and daemon
const abort = new AbortController()
process.on('SIGTERM', () => abort.abort())
process.on('SIGINT', () => abort.abort())
await runBridgeHeadless(opts, abort.signal)
// signal.aborted === true triggers poll loop teardown
```

### 8. Atomic File Operations

```typescript
// ✅ Write to .tmp then rename — crash-safe
const tmp = path + '.tmp'
await writeFile(tmp, content, 'utf8')
await rename(tmp, path)
```

---

## Architecture Seams — Where to Extend

| Seam | Interface | Location | Extension Point |
|------|-----------|----------|-----------------|
| Session spawning | `SessionSpawner` | `bridge/types.ts:209` | Replace subprocess with in-process sessions |
| UI rendering | `BridgeLogger` | `bridge/types.ts:213` | Replace ANSI TUI with web UI, JSON, etc. |
| API communication | `BridgeApiClient` | `bridge/types.ts:133` | Mock for tests, replace with custom backend |
| Daemon worker entry | `HeadlessBridgeOpts` | `bridge/bridgeMain.ts:2785` | Inject custom auth, logging, permission callbacks |
| Permission decisions | `PermissionRouter` | `daemon/permissionRouter.ts` | Custom auto-approve policies, audit logging |
| Token lifecycle | `AuthManager` | `daemon/authManager.ts` | Custom token providers, MFA integration |
| Worker management | `WorkerRegistry` | `daemon/workerRegistry.ts` | Custom spawn strategies, resource limits |
| IPC transport | `Connection` | `daemon/socketServer.ts` | Replace Unix socket with TCP, WebSocket, etc. |
| OS notifications | `sendOsNotification` | `daemon/osNotification.ts` | Webhook, Slack, email notifications |

---

## Daemon Mode — Quick Reference

### Process Hierarchy

```
claude --daemon-supervisor        (1 per machine, long-lived)
  └── claude --daemon-worker      (1 per directory, spawned by supervisor)
        └── claude --print ...    (1 per session, spawned by runBridgeHeadless)
```

### IPC Flow

```
Client ──(Unix socket)──> Supervisor ──(stdio pipes)──> Worker ──(stdio pipes)──> Session
         ~/.claude/daemon.sock       NDJSON frames              NDJSON frames
```

### Key Files

| File | Lines | Role |
|------|-------|------|
| `daemon/supervisor.ts` | 520 | Unix socket server, orchestration, systemd notify |
| `daemon/workerRegistry.ts` | 449 | Process pool: spawn, monitor, backoff, park, idle shutdown |
| `daemon/client.ts` | 421 | CLI: start, stop, status, logs, approve, deny, install-service |
| `daemon/worker.ts` | 250 | Worker entry, wraps runBridgeHeadless(), IPC relay |
| `daemon/ipcProtocol.ts` | 205 | Types + frame codec (4-byte prefix + JSON) |
| `daemon/permissionRouter.ts` | 199 | TTL broker, multi-client broadcast, attended tracking |
| `daemon/socketServer.ts` | 195 | Unix socket abstraction, peer credential check |
| `daemon/conversationCache.ts` | 167 | LRU cache (100 msg/session, 32 sessions) |
| `daemon/authManager.ts` | 135 | OAuth wrap, in-memory token, broadcast updates |
| `daemon/daemonConfig.ts` | 105 | Zod schema for ~/.claude/daemon.json |
| `daemon/ringBuffer.ts` | 41 | Generic fixed-size ring buffer |
| `daemon/pidFile.ts` | 67 | Atomic PID file management |
| `daemon/osNotification.ts` | 38 | Platform desktop notifications |
| `daemon/index.ts` | 97 | Barrel exports |

### Config: `~/.claude/daemon.json`

```json
{
  "version": 1,
  "socketPath": "~/.claude/daemon.sock",
  "pidPath": "~/.claude/daemon.pid",
  "logPath": "~/.claude/daemon.log",
  "maxWorkersPerHost": 8,
  "maxSessionsPerWorker": 32,
  "permissionTtlMs": 120000,
  "unattendedBehavior": "deny",
  "idleWorkerShutdownMs": 3600000,
  "prewarmSessions": true,
  "persistedWorkers": []
}
```

---

## Environment Variables

### Bridge / Worker

| Variable | Purpose |
|----------|---------|
| `CLAUDE_CODE_ENVIRONMENT_KIND=bridge` | Marks child as bridge worker |
| `CLAUDE_CODE_SESSION_ACCESS_TOKEN` | Session ingress JWT (per-session) |
| `CLAUDE_CODE_USE_CCR_V2=1` | Use SSE transport (CCR v2) |
| `CLAUDE_CODE_POST_FOR_SESSION_INGRESS_V2=1` | Use Hybrid transport (v1) |
| `CLAUDE_CODE_WORKER_EPOCH` | Worker epoch from registration |
| `CLAUDE_CODE_FORCE_SANDBOX=1` | Force sandbox mode |
| `CLAUDE_CODE_STREAMLINED_OUTPUT=true` | Compact output |
| `CLAUDE_CODE_PROACTIVE=1` | Proactive agent mode |

### Dev Overrides (USER_TYPE === 'ant' only)

| Variable | Purpose |
|----------|---------|
| `CLAUDE_BRIDGE_BASE_URL` | Override API base URL |
| `CLAUDE_BRIDGE_SESSION_INGRESS_URL` | Override session ingress URL |
| `CLAUDE_BRIDGE_USE_CCR_V2` | Force CCR v2 mode |
| `CLAUDE_BRIDGE_OAUTH_TOKEN` | Dev token override |

### systemd

| Variable | Purpose |
|----------|---------|
| `NOTIFY_SOCKET` | sd_notify socket (auto-set by systemd) |

---

## Global State Constraints — CRITICAL

These are **per-process singletons**. Violating them causes undefined behavior:

| Singleton | Location | Constraint |
|-----------|----------|------------|
| `process.chdir(dir)` | `bridgeMain.ts:2819` | One CWD per process. Workers MUST be separate processes. |
| `setOriginalCwd(dir)` | `bootstrap/state.js` | Set once at startup. Cannot change. |
| `setCwdState(dir)` | `bootstrap/state.js` | Set once at startup. Cannot change. |
| `enableConfigs()` | `utils/config.js` | Enables config loading. One-shot. |
| `initSinks()` | `utils/sinks.js` | Analytics initialization. One-shot. |

**Consequence**: The supervisor MUST NEVER import from `bridge/`, `utils/config`, or `bootstrap/state`. Each worker is a separate OS process. No threading, no in-process worker pool.

---

## Security Model

| Layer | Mechanism |
|-------|-----------|
| Socket access | `~/.claude/` directory mode 0700 (owner-only) |
| Peer verification | `SO_PEERCRED` UID check on Linux |
| Socket permissions | `chmod 0600` on daemon.sock after creation |
| Token isolation | Workers receive tokens via IPC, never via CLI args or env leakage |
| Env stripping | `CLAUDE_CODE_OAUTH_TOKEN: undefined` in worker child env |
| PID file | Atomic write (tmp + rename), stale detection via `kill(pid, 0)` |
| systemd hardening | `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome=read-only` |
| Resource limits | `MemoryMax=512M`, `TasksMax=256` (systemd) |

---

## Crash Recovery

1. Supervisor crash → workers detect **pipe EOF** → graceful shutdown → write bridge-pointer.json
2. Next `claude daemon start` → reads `daemon.json` → restores `persistedWorkers[]` → respawns
3. Worker crash → supervisor detects exit code:
   - `EXIT_CODE_PERMANENT` (78) → **park** (no restart, log error)
   - `EXIT_CODE_TRANSIENT` (1) → **backoff retry** (500ms → 30s cap, park after 3 fast failures)
   - `EXIT_CODE_OK` (0) → **clean exit** (remove from registry)
4. Session crash → child process exits → `SessionHandle.done` resolves → bridge archives session

---

## Performance Targets

| Metric | Target |
|--------|--------|
| Warm session start (daemon hot) | < 5ms |
| Cold session start (new dir) | < 400ms |
| Permission round-trip (client attached) | < 100ms |
| Supervisor memory | < 30MB RSS |
| IPC frame decode throughput | > 10,000 frames/sec |
| Max concurrent workers | 8 (configurable) |
| Max sessions per worker | 32 (SPAWN_SESSIONS_DEFAULT) |

---

## Rules for AI Agents Working in This Codebase

1. **Always use `.js` extensions** in relative imports. ESM requires it.
2. **Never import bridge/ or utils/ from daemon/supervisor.ts**. Global state contamination.
3. **Follow injection pattern**. Create factory functions that accept deps, not classes with hard imports.
4. **Use Zod for schemas**. Match the `lazySchema` pattern from bridgePointer.ts.
5. **Classify errors**. Permanent → `BridgeHeadlessPermanentError`. Transient → plain `Error`.
6. **Use ring buffers** for bounded collections (logs, activities, dedup sets).
7. **Graceful shutdown** via `AbortController`. Never call `process.exit()` in library code.
8. **Atomic file writes**. Write to `.tmp`, then `rename()`.
9. **No test files exist** in this repo. When writing tests, use Bun's built-in test runner (`bun test`).
10. **Feature flags** are compile-time constants in production. Use `feature('FLAG_NAME')` from `bun:bundle`.
11. **NDJSON everywhere**. stdio pipes, IPC frames, transcripts — all use newline-delimited JSON.
12. **Timers must `.unref()`** if they shouldn't prevent process exit.
13. **Backoff on retry**. Use `BackoffConfig` type. Never busy-loop or fixed-delay retry.
14. **Strip secrets from child env**. Set `CLAUDE_CODE_OAUTH_TOKEN: undefined` when spawning.
15. **Bridge pointer is the resume mechanism**. Write `bridge-pointer.json` with `source: 'daemon'` for daemon workers.
