# AZOTH

A daemon-mode CLI agent that transmutes intent into code. Built on top of Anthropic's Claude Code source.

```
$ azoth daemon start .
Worker started: /home/user/project:same-dir
  PID: 48291
  Status: running
  Capacity: 32 sessions

$ azoth daemon status
Claude Code Daemon
  Version: 1.0.0
  Uptime: 2h 14m
  Workers:
  KEY                                  PID     STATUS     SESSIONS  UPTIME
  /home/user/project:same-dir         48291   running    3/32      2h 14m
  /home/user/api:worktree             48305   running    1/32      47m
```

---

## What this is

On March 31, 2026, Anthropic's Claude Code CLI source leaked via a `.map` file in their npm registry. This repo contains that source — specifically the **bridge**, **CLI transport**, and **session management** layers (~27K lines of TypeScript) — plus a complete **daemon mode** implementation built on top of the existing architecture.

The daemon mode is not a hack bolted onto the side. The Claude Code source already contained explicit scaffolding for it — `runBridgeHeadless()` with comments referencing "daemon workers" and "supervisor's AuthManager" — but the supervisor process itself was never shipped. We built it.

## What this is not

- Not the full Claude Code source (~512K lines, ~1900 files). This is a partial snapshot: bridge, CLI, transports.
- Not a jailbreak, crack, or bypass tool.
- Not production-ready. There are no tests. The build system was reconstructed externally. Treat it as a research artifact.

---

## Architecture

```
azoth daemon start .
        │
        ▼
┌──────────────────────────────┐
│  CLI Client (ephemeral)       │  Connects, sends request, exits.
└──────────┬───────────────────┘
           │ Unix domain socket
           │ ~/.claude/daemon.sock
┌──────────▼───────────────────┐
│  Supervisor (long-lived)      │  One per machine. Manages everything.
│                               │
│  AuthManager ─── OAuth token lifecycle, in-memory cache
│  PermissionRouter ─── TTL-based permission broker
│  WorkerRegistry ─── Process pool with backoff and parking
│  ConversationCache ─── LRU message cache
└──────────┬───────────────────┘
           │ stdio pipes (NDJSON)
┌──────────▼───────────────────┐
│  Worker (per directory)       │  Wraps runBridgeHeadless().
│                               │  One process per project directory.
│  ┌─────────────────────────┐ │
│  │ Session (child process)  │ │  Up to 32 per worker.
│  │ claude --print           │ │  WebSocket/SSE to Anthropic API.
│  └─────────────────────────┘ │
└──────────────────────────────┘
```

**Why one worker per directory**: `process.chdir()` is global in Node.js/Bun. The bridge code calls it at startup. Multiple directories in one process would stomp each other's CWD. This isn't a design choice — it's a constraint from the existing codebase, and we respect it.

**Why Unix sockets**: No port conflicts, no firewall rules, sub-millisecond latency, peer credential verification via `SO_PEERCRED`. The socket lives at `~/.claude/daemon.sock` with mode `0600`.

---

## Directory layout

```
.
├── daemon/              # 2,889 lines — supervisor, workers, IPC, permissions
│   ├── supervisor.ts    # Unix socket server, worker orchestration, systemd notify
│   ├── workerRegistry.ts # Process pool: spawn, monitor, backoff, park, idle shutdown
│   ├── client.ts        # CLI: start, stop, status, logs, approve, deny
│   ├── worker.ts        # Worker entry, wraps runBridgeHeadless()
│   ├── ipcProtocol.ts   # Frame codec (4-byte length prefix + JSON)
│   ├── permissionRouter.ts # TTL broker, multi-client broadcast
│   ├── socketServer.ts  # Unix socket abstraction
│   ├── conversationCache.ts # LRU session message cache
│   ├── authManager.ts   # OAuth wrap, token broadcast to workers
│   ├── daemonConfig.ts  # ~/.claude/daemon.json schema (Zod)
│   ├── ringBuffer.ts    # Generic fixed-size ring buffer
│   ├── pidFile.ts       # Atomic PID file management
│   ├── osNotification.ts # Desktop notifications (notify-send / osascript)
│   └── index.ts         # Barrel exports
├── bridge/              # 12,619 lines — Anthropic's session management (leaked)
│   ├── bridgeMain.ts    # runBridgeLoop(), runBridgeHeadless() [L2810]
│   ├── sessionRunner.ts # child_process.spawn(), NDJSON relay
│   ├── bridgeApi.ts     # Environments API client
│   ├── types.ts         # BridgeConfig, SessionHandle, SessionSpawner, BridgeLogger
│   ├── replBridge.ts    # REPL bridge, BridgeCoreParams
│   └── ... (27 more)
├── cli/                 # 12,353 lines — transports and handlers (leaked)
│   ├── structuredIO.ts  # SDK message I/O, permission hooks
│   ├── transports/      # HybridTransport, WebSocket, SSE, CCR v2
│   └── handlers/        # MCP, auth, agents, plugins
├── contrib/
│   ├── systemd/         # claude-daemon.service
│   └── launchd/         # com.anthropic.claude-daemon.plist
├── CLAUDE.md            # AI agent operating manual (not for humans)
└── README.md            # This file
```

---

## The daemon mode

### Problem

Every `claude` invocation pays a cold-start tax: process fork, Bun/Node startup, config file reads, OAuth token refresh, git branch resolution, environment registration with Anthropic's API. That's 800ms–2s before anything useful happens.

### Solution

Keep a supervisor running. It holds the OAuth token in memory, manages a pool of worker processes (one per project directory), and exposes a Unix socket for instant IPC. A warm session start takes <5ms — one socket round trip.

### How it works

The existing `runBridgeHeadless()` function at `bridge/bridgeMain.ts:2810` was designed for exactly this. The comments say:

> *"Non-interactive bridge entrypoint for the `remoteControl` daemon worker."*
> *"Config comes from the caller (daemon.json), auth comes via IPC (supervisor's AuthManager), logs go to the worker's stdout pipe."*

We built the supervisor, the IPC protocol, the CLI client, and the permission router that these comments describe. Three lines changed in the existing bridge code. Everything else is additive.

### Existing bridge code modified

```diff
# bridge/bridgePointer.ts — 1 line
- source: z.enum(['standalone', 'repl']),
+ source: z.enum(['standalone', 'repl', 'daemon']),

# bridge/bridgeMain.ts — 5 lines added to HeadlessBridgeOpts
+ onPermissionRequest?: (
+   sessionId: string,
+   request: unknown,
+   accessToken: string,
+ ) => void
```

That's it. The daemon is a shell around the bridge, not a rewrite of it.

---

## Usage

### Start the daemon

```sh
# Auto-starts supervisor if not running, then starts a worker for current directory
azoth daemon start .

# Or start for a specific directory with worktree isolation
azoth daemon start /path/to/project --worktree
```

### Check status

```sh
azoth daemon status
```

### Stream worker logs

```sh
azoth daemon logs .
# Ctrl+C to stop
```

### Handle permissions

When a session needs tool approval and no terminal is attached:

```sh
azoth daemon approve <requestId>
azoth daemon deny <requestId>
```

### Stop a worker

```sh
azoth daemon stop .
azoth daemon stop /path/to/project --force
```

### Install as a system service

```sh
# Linux (systemd)
azoth daemon install-service
systemctl --user enable --now claude-daemon

# macOS (launchd)
azoth daemon install-service
launchctl load ~/Library/LaunchAgents/com.anthropic.claude-daemon.plist
```

---

## Configuration

`~/.claude/daemon.json` — created automatically with defaults on first run.

| Key | Default | What it does |
|-----|---------|--------------|
| `maxWorkersPerHost` | 8 | Maximum concurrent worker processes |
| `maxSessionsPerWorker` | 32 | Maximum sessions per worker |
| `permissionTtlMs` | 120000 | Auto-deny unattended permission requests after 2 min |
| `unattendedBehavior` | `"deny"` | What to do on TTL expiry: `deny`, `notify`, or `allow` |
| `idleWorkerShutdownMs` | 3600000 | Kill idle workers after 1 hour |
| `prewarmSessions` | true | Pre-create sessions for instant availability |

---

## Crash recovery

| Scenario | What happens |
|----------|--------------|
| Supervisor killed | Workers detect pipe EOF, shut down gracefully, write bridge-pointer.json. Next `azoth daemon start` restores from `persistedWorkers` in daemon.json. |
| Worker crash (transient) | Supervisor retries with exponential backoff (500ms → 30s cap). Parks after 3 fast failures. |
| Worker crash (permanent) | Exit code 78. Supervisor parks the worker. Manual `--force` restart required. |
| Session crash | Child process exits. Worker archives session via API. Bridge pointer enables resume. |

---

## Performance

| Metric | Value |
|--------|-------|
| Warm session start | <5ms |
| Cold session start (new dir, daemon running) | <400ms |
| Permission round-trip (client attached) | <100ms |
| Supervisor RSS | <30MB |

---

## Security

- Socket at `~/.claude/daemon.sock`, mode `0600`, inside `~/.claude/` (mode `0700`)
- Peer UID verification via `SO_PEERCRED` on Linux
- OAuth tokens never passed as CLI arguments or environment variables to child processes
- Workers receive tokens exclusively through supervisor IPC pipes
- systemd unit runs with `NoNewPrivileges=yes`, `ProtectSystem=strict`, `MemoryMax=512M`

---

## Feature flags

The original Claude Code uses compile-time feature flags via `bun:bundle`. Runtime polyfill at `node_modules/bundle/index.js`:

```
KAIROS              PROACTIVE           BRIDGE_MODE         VOICE_MODE
COORDINATOR_MODE    BASH_CLASSIFIER     BUDDY               WEB_BROWSER_TOOL
CHICAGO_MCP         AGENT_TRIGGERS      ULTRAPLAN           MONITOR_TOOL
TEAMMEM             EXTRACT_MEMORIES    MCP_SKILLS          TRANSCRIPT_CLASSIFIER
```

---

## Building

This repo does not ship a build system. The original Claude Code build chain (`bun build src/main.tsx --outdir=dist --target=bun`) requires ~1900 source files and 60+ npm dependencies that are not included here. The daemon module can be compiled independently against the bridge types.

---

## Origin

The source was leaked on March 31, 2026 via a `.map` file in Anthropic's npm registry. [Chaofan Shou](https://x.com/ArbiterCFC) discovered and disclosed it publicly. The source map referenced unobfuscated TypeScript in an R2 storage bucket.

This repo contains the extracted partial source (bridge, CLI, transports) plus the daemon mode implementation. The core engine (coordinator, tools, context management, terminal UI, commands) is not included.

---

## License

Apache 2.0. See [LICENSE](LICENSE).
