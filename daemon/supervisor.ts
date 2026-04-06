/**
 * Daemon supervisor — the main long-running process for Claude Code daemon mode.
 *
 * Responsibilities:
 * - Owns the Unix domain socket at ~/.claude/daemon.sock
 * - Routes IPC requests between CLI clients and worker processes
 * - Manages AuthManager (single OAuth token lifecycle)
 * - Manages PermissionRouter (TTL-based permission broker)
 * - Manages WorkerRegistry (spawn, monitor, restart, park)
 * - Handles systemd/launchd integration (sd_notify, watchdog)
 * - Persists worker list to daemon.json for restart recovery
 *
 * Import restriction: This module only imports from node: builtins and daemon/*.
 * It NEVER imports from bridge/, utils/config, or bootstrap/state to avoid
 * pulling in global singletons that conflict with worker processes.
 *
 * Entry: claude --daemon-supervisor
 */

import { homedir } from 'os'
import { join } from 'path'

import { createSocketServer, type Connection, type DaemonSocketServer } from './socketServer.js'
import { createWorkerRegistry, type WorkerRegistry } from './workerRegistry.js'
import { createAuthManager, type AuthManager } from './authManager.js'
import {
  createPermissionRouter,
  type PermissionRouter,
} from './permissionRouter.js'
import {
  readDaemonConfig,
  writeDaemonConfig,
  persistWorker,
  unpersistWorker,
  type DaemonConfig,
} from './daemonConfig.js'
import { writePidFile, removePidFile, clearStalePidFile } from './pidFile.js'
import {
  type DaemonRequest,
  type DaemonResponse,
  type StartWorkerPayload,
  type StopWorkerPayload,
  type PermissionResponsePayload,
  type SubscribeLogsPayload,
  type StatusResponse,
  workerKey,
  nextRequestId,
} from './ipcProtocol.js'

// ─── Supervisor state ─────────────────────────────────────────────────────────

let config: DaemonConfig
let socketServer: DaemonSocketServer
let workerRegistry: WorkerRegistry
let authManager: AuthManager
let permissionRouter: PermissionRouter
let startedAt: number

// Track which connections are subscribed to which worker logs
const logSubscriptions = new Map<string, Set<Connection>>()

// ─── Main ─────────────────────────────────────────────────────────────────────

export async function runSupervisor(): Promise<void> {
  startedAt = Date.now()
  log('[supervisor] starting...')

  // 1. Read config
  config = await readDaemonConfig()
  log(`[supervisor] config loaded: maxWorkers=${config.maxWorkersPerHost}`)

  // 2. Check for stale PID file
  const pidCleared = await clearStalePidFile(config.pidPath)
  if (!pidCleared) {
    log('[supervisor] another daemon is already running')
    process.exit(1)
  }

  // 3. Write our PID
  await writePidFile(config.pidPath)

  // 4. Initialize AuthManager
  authManager = createAuthManager()
  authManager.subscribeToTokenUpdates((token) => {
    // Broadcast token updates to all workers
    workerRegistry.broadcastToken(token)
  })
  authManager.startRefreshLoop()
  log('[supervisor] auth manager started')

  // 5. Initialize PermissionRouter
  permissionRouter = createPermissionRouter({
    defaultTtlMs: config.permissionTtlMs,
    unattendedBehavior: config.unattendedBehavior,
    broadcastToClients(msg: DaemonResponse): void {
      socketServer.broadcast(msg)
    },
  })
  log('[supervisor] permission router started')

  // 6. Initialize WorkerRegistry
  workerRegistry = createWorkerRegistry({
    execPath: process.execPath,
    scriptArgs: process.argv.slice(1).filter((a) => a !== '--daemon-supervisor').concat('--daemon-worker'),
    maxWorkers: config.maxWorkersPerHost,
    idleShutdownMs: config.idleWorkerShutdownMs,
    getToken: () => authManager.getAccessToken(),
    log,
    onPermissionRequest(request) {
      // Route permission request through the router → IPC clients
      permissionRouter.addPendingRequest(request).then((response) => {
        // Forward resolution back to the worker
        workerRegistry.sendPermissionResolution(
          request.workerKey,
          request.requestId,
          response.behavior,
        )
      })
    },
    async onAuth401(failedToken: string): Promise<boolean> {
      return authManager.onAuth401(failedToken)
    },
  })
  log('[supervisor] worker registry started')

  // 7. Create Unix socket server
  socketServer = await createSocketServer({
    socketPath: config.socketPath,
    onConnection: handleClientConnection,
  })
  await socketServer.listen()
  log(`[supervisor] listening on ${config.socketPath}`)

  // 8. Restore persisted workers
  for (const pw of config.persistedWorkers) {
    try {
      workerRegistry.getOrSpawn({
        dir: pw.dir,
        spawnMode: pw.spawnMode,
        capacity: pw.capacity,
        sandbox: pw.sandbox,
        prewarm: config.prewarmSessions,
      })
      log(`[supervisor] restored worker: ${pw.dir}:${pw.spawnMode}`)
    } catch (err: unknown) {
      log(
        `[supervisor] failed to restore worker ${pw.dir}: ${(err as Error)?.message}`,
      )
    }
  }

  // 9. Set up signal handlers
  const shutdown = async () => {
    log('[supervisor] shutting down...')

    // Stop accepting new connections
    await socketServer.close()

    // Gracefully stop all workers
    await workerRegistry.stopAll(30_000)

    // Clean up
    authManager.stopRefreshLoop()
    permissionRouter.clear()
    await removePidFile(config.pidPath)

    log('[supervisor] shutdown complete')
    process.exit(0)
  }

  process.on('SIGTERM', shutdown)
  process.on('SIGINT', shutdown)

  // 10. systemd integration
  notifySystemd('READY=1')

  // Watchdog keepalive (every 30s)
  const watchdogTimer = setInterval(() => {
    notifySystemd('WATCHDOG=1')
  }, 30_000)
  watchdogTimer.unref()

  log('[supervisor] ready')

  // Keep process alive
  await new Promise(() => {
    // Never resolves — process runs until signal
  })
}

// ─── IPC request handler ──────────────────────────────────────────────────────

function handleClientConnection(conn: Connection): void {
  log(`[supervisor] client connected: ${conn.id}`)

  // Send any pending permission requests to the new client
  const pending = permissionRouter.getPendingRequests()
  for (const req of pending) {
    conn.send({
      id: req.requestId,
      type: 'permission_request',
      payload: req,
    } satisfies DaemonResponse)
  }

  conn.onMessage((raw) => {
    const req = raw as DaemonRequest
    if (!req?.id || !req?.type) {
      conn.send({ id: 'unknown', type: 'error', payload: { message: 'Invalid request' } })
      return
    }
    handleRequest(conn, req)
  })

  conn.onClose(() => {
    log(`[supervisor] client disconnected: ${conn.id}`)
    // Clean up log subscriptions
    for (const [key, subs] of logSubscriptions) {
      subs.delete(conn)
      if (subs.size === 0) {
        logSubscriptions.delete(key)
        permissionRouter.markUnattended(key)
      }
    }
  })
}

function handleRequest(conn: Connection, req: DaemonRequest): void {
  try {
    switch (req.type) {
      case 'ping':
        conn.send({ id: req.id, type: 'pong' } satisfies DaemonResponse)
        break

      case 'status':
        handleStatus(conn, req)
        break

      case 'start_worker':
        handleStartWorker(conn, req)
        break

      case 'stop_worker':
        handleStopWorker(conn, req)
        break

      case 'list_workers':
        handleListWorkers(conn, req)
        break

      case 'permission_response':
        handlePermissionResponse(conn, req)
        break

      case 'subscribe_worker_logs':
        handleSubscribeLogs(conn, req)
        break

      case 'unsubscribe_worker_logs':
        handleUnsubscribeLogs(conn, req)
        break

      case 'upgrade_check':
        handleUpgradeCheck(conn, req)
        break

      default:
        conn.send({
          id: req.id,
          type: 'error',
          payload: { message: `Unknown request type: ${req.type}` },
        } satisfies DaemonResponse)
    }
  } catch (err: unknown) {
    conn.send({
      id: req.id,
      type: 'error',
      payload: { message: (err as Error)?.message ?? String(err) },
    } satisfies DaemonResponse)
  }
}

// ─── Request handlers ─────────────────────────────────────────────────────────

function handleStatus(conn: Connection, req: DaemonRequest): void {
  const status: StatusResponse = {
    version: process.env.CLAUDE_CODE_VERSION ?? 'unknown',
    uptime: Date.now() - startedAt,
    workers: workerRegistry.listWorkers(),
    pendingPermissions: permissionRouter.pendingCount(),
  }
  conn.send({
    id: req.id,
    type: 'status_response',
    payload: status,
  } satisfies DaemonResponse)
}

function handleStartWorker(conn: Connection, req: DaemonRequest): void {
  const payload = req.payload as StartWorkerPayload
  if (!payload?.dir) {
    conn.send({
      id: req.id,
      type: 'error',
      payload: { message: 'Missing dir in start_worker payload' },
    })
    return
  }

  const handle = workerRegistry.getOrSpawn({
    dir: payload.dir,
    spawnMode: payload.spawnMode ?? 'same-dir',
    capacity: payload.capacity ?? config.maxSessionsPerWorker,
    sandbox: payload.sandbox ?? false,
    sessionTimeoutMs: payload.sessionTimeoutMs,
    prewarm: payload.prewarm ?? config.prewarmSessions,
    permissionMode: payload.permissionMode,
  })

  // Wire log forwarding to this client
  const key = handle.key
  handle.onLog = (line) => {
    const subs = logSubscriptions.get(key)
    if (subs) {
      const msg: DaemonResponse = {
        id: nextRequestId(),
        type: 'worker_log',
        payload: { workerKey: key, message: line },
      }
      for (const sub of subs) {
        sub.send(msg)
      }
    }
  }

  // Persist worker for restart recovery
  persistWorker({
    dir: payload.dir,
    spawnMode: payload.spawnMode ?? 'same-dir',
    capacity: payload.capacity ?? config.maxSessionsPerWorker,
    sandbox: payload.sandbox ?? false,
  })

  const info = workerRegistry.listWorkers().find((w) => w.key === key)
  conn.send({
    id: req.id,
    type: 'worker_started',
    payload: info,
  } satisfies DaemonResponse)
}

async function handleStopWorker(
  conn: Connection,
  req: DaemonRequest,
): Promise<void> {
  const payload = req.payload as StopWorkerPayload
  if (!payload?.dir) {
    conn.send({
      id: req.id,
      type: 'error',
      payload: { message: 'Missing dir in stop_worker payload' },
    })
    return
  }

  const key = workerKey(payload.dir, payload.spawnMode ?? 'same-dir')
  await workerRegistry.stop(key, payload.force)

  // Remove from persisted workers
  await unpersistWorker(payload.dir, payload.spawnMode ?? 'same-dir')

  conn.send({
    id: req.id,
    type: 'worker_stopped',
    payload: { key },
  } satisfies DaemonResponse)
}

function handleListWorkers(conn: Connection, req: DaemonRequest): void {
  conn.send({
    id: req.id,
    type: 'worker_list',
    payload: workerRegistry.listWorkers(),
  } satisfies DaemonResponse)
}

function handlePermissionResponse(
  conn: Connection,
  req: DaemonRequest,
): void {
  const payload = req.payload as PermissionResponsePayload
  if (!payload?.requestId || !payload?.behavior) {
    conn.send({
      id: req.id,
      type: 'error',
      payload: { message: 'Missing requestId or behavior' },
    })
    return
  }

  const resolved = permissionRouter.resolveRequest(
    payload.requestId,
    payload,
  )
  conn.send({
    id: req.id,
    type: resolved ? 'ok' : 'error',
    payload: resolved
      ? { message: 'Permission resolved' }
      : { message: 'Unknown or expired requestId' },
  } satisfies DaemonResponse)
}

function handleSubscribeLogs(conn: Connection, req: DaemonRequest): void {
  const payload = req.payload as SubscribeLogsPayload
  if (!payload?.workerKey) {
    conn.send({
      id: req.id,
      type: 'error',
      payload: { message: 'Missing workerKey' },
    })
    return
  }

  // Add to subscriptions
  let subs = logSubscriptions.get(payload.workerKey)
  if (!subs) {
    subs = new Set()
    logSubscriptions.set(payload.workerKey, subs)
  }
  subs.add(conn)

  // Mark worker as attended (suppress permission TTL auto-deny)
  permissionRouter.markAttended(payload.workerKey)

  // Replay log buffer
  const handle = workerRegistry.get(payload.workerKey)
  if (handle) {
    const history = handle.logBuffer.toArray()
    for (const line of history) {
      conn.send({
        id: nextRequestId(),
        type: 'worker_log',
        payload: { workerKey: payload.workerKey, message: line },
      } satisfies DaemonResponse)
    }
  }

  conn.send({
    id: req.id,
    type: 'ok',
    payload: { message: `Subscribed to ${payload.workerKey}` },
  } satisfies DaemonResponse)
}

function handleUnsubscribeLogs(conn: Connection, req: DaemonRequest): void {
  const payload = req.payload as SubscribeLogsPayload
  if (payload?.workerKey) {
    const subs = logSubscriptions.get(payload.workerKey)
    if (subs) {
      subs.delete(conn)
      if (subs.size === 0) {
        logSubscriptions.delete(payload.workerKey)
        permissionRouter.markUnattended(payload.workerKey)
      }
    }
  }
  conn.send({
    id: req.id,
    type: 'ok',
  } satisfies DaemonResponse)
}

function handleUpgradeCheck(conn: Connection, req: DaemonRequest): void {
  conn.send({
    id: req.id,
    type: 'status_response',
    payload: {
      version: process.env.CLAUDE_CODE_VERSION ?? 'unknown',
      execPath: process.execPath,
      pid: process.pid,
      uptime: Date.now() - startedAt,
    },
  } satisfies DaemonResponse)
}

// ─── systemd notify ───────────────────────────────────────────────────────────

function notifySystemd(state: string): void {
  const notifySocket = process.env.NOTIFY_SOCKET
  if (!notifySocket) return

  try {
    // Use dgram to send to the systemd notification socket
    import('dgram').then(({ createSocket }) => {
      const client = createSocket('unix_dgram')
      client.send(state, notifySocket, (err) => {
        client.close()
      })
    })
  } catch {
    // Non-critical
  }
}

// ─── Logging ──────────────────────────────────────────────────────────────────

function log(msg: string): void {
  const ts = new Date().toISOString()
  process.stderr.write(`${ts} ${msg}\n`)
}

// ─── Entry point ──────────────────────────────────────────────────────────────

if (process.argv.includes('--daemon-supervisor')) {
  runSupervisor().catch((err) => {
    log(`[supervisor] fatal: ${err?.message ?? err}`)
    process.exit(1)
  })
}
