/**
 * Worker process manager for the daemon supervisor.
 *
 * Manages a Map of running worker processes keyed by (dir + spawnMode).
 * Handles spawn, monitor, restart with backoff, parking on permanent errors,
 * and idle shutdown.
 *
 * Each worker is a separate OS process running daemon/worker.ts, which
 * internally calls the existing runBridgeHeadless(). This honors the
 * global-state constraint (process.chdir, enableConfigs are per-process).
 */

import { spawn, ChildProcess } from 'child_process'
import { createInterface } from 'readline'
import { RingBuffer } from './ringBuffer.js'
import {
  type WorkerIpcMessage,
  type WorkerInfo,
  type WorkerStatus,
  type StartWorkerPayload,
  type PermissionRequestPayload,
  workerKey,
  EXIT_CODE_PERMANENT,
} from './ipcProtocol.js'

// ─── Types ────────────────────────────────────────────────────────────────────

export type WorkerHandle = {
  key: string
  opts: StartWorkerPayload
  process: ChildProcess
  pid: number
  status: WorkerStatus
  startedAt: number
  activeSessions: number
  consecutiveFailures: number
  backoffMs: number
  restartTimer: ReturnType<typeof setTimeout> | null
  logBuffer: RingBuffer<string>
  /** Called when the worker reports a permission request. */
  onPermissionRequest?: (request: PermissionRequestPayload) => void
  /** Called when the worker sends a log line. */
  onLog?: (line: string) => void
  /** Called when the worker reports an auth 401. */
  onAuth401?: (failedToken: string) => Promise<boolean>
  /** Called when the worker exits. */
  onExit?: (code: number | null, signal: string | null) => void
}

export type WorkerRegistryOpts = {
  /** Path to the claude binary (process.execPath). */
  execPath: string
  /** Script args to invoke the worker (e.g., ['--daemon-worker']). */
  scriptArgs: string[]
  /** Maximum workers allowed. */
  maxWorkers: number
  /** Idle shutdown timeout (ms). */
  idleShutdownMs: number
  /** Initial OAuth token to pass to workers. */
  getToken: () => string | undefined
  /** Log callback for supervisor-level messages. */
  log: (msg: string) => void
  /** Called when a worker emits a permission request. */
  onPermissionRequest: (request: PermissionRequestPayload) => void
  /** Called when a worker needs token refresh. */
  onAuth401: (failedToken: string) => Promise<boolean>
}

export type WorkerRegistry = {
  /** Get or spawn a worker for the given options. */
  getOrSpawn(opts: StartWorkerPayload): WorkerHandle
  /** Stop a specific worker. */
  stop(key: string, force?: boolean): Promise<void>
  /** Stop all workers (graceful shutdown). */
  stopAll(graceMs?: number): Promise<void>
  /** Get info about all workers. */
  listWorkers(): WorkerInfo[]
  /** Get a specific worker handle. */
  get(key: string): WorkerHandle | undefined
  /** Check if a worker exists. */
  has(key: string): boolean
  /** Send a token update to all running workers. */
  broadcastToken(token: string): void
  /** Send a permission resolution to a specific worker. */
  sendPermissionResolution(
    workerKeyStr: string,
    requestId: string,
    behavior: 'allow' | 'deny',
  ): void
  /** Number of active workers. */
  size: number
}

// ─── Backoff config (matches bridge/bridgeMain.ts:59) ─────────────────────────

const BACKOFF_INITIAL_MS = 500
const BACKOFF_CAP_MS = 30_000
const BACKOFF_FAST_FAILURE_THRESHOLD_MS = 10_000
const BACKOFF_MAX_FAST_FAILURES = 3

// ─── Implementation ───────────────────────────────────────────────────────────

export function createWorkerRegistry(
  registryOpts: WorkerRegistryOpts,
): WorkerRegistry {
  const workers = new Map<string, WorkerHandle>()
  const idleTimers = new Map<string, ReturnType<typeof setTimeout>>()

  function spawnWorker(opts: StartWorkerPayload): WorkerHandle {
    const key = workerKey(opts.dir, opts.spawnMode)

    registryOpts.log(`[registry] spawning worker: ${key}`)

    const child = spawn(
      registryOpts.execPath,
      registryOpts.scriptArgs,
      {
        cwd: opts.dir,
        stdio: ['pipe', 'pipe', 'pipe'],
        env: {
          ...process.env,
          // Strip sensitive vars — worker gets token via IPC
          CLAUDE_CODE_OAUTH_TOKEN: undefined,
          CLAUDE_CODE_OAUTH_REFRESH_TOKEN: undefined,
        },
      },
    )

    // Send config as first line on stdin
    const config = {
      dir: opts.dir,
      spawnMode: opts.spawnMode,
      capacity: opts.capacity,
      sandbox: opts.sandbox,
      sessionTimeoutMs: opts.sessionTimeoutMs,
      prewarm: opts.prewarm,
      permissionMode: opts.permissionMode,
      initialToken: registryOpts.getToken(),
    }
    child.stdin!.write(JSON.stringify(config) + '\n')

    const handle: WorkerHandle = {
      key,
      opts,
      process: child,
      pid: child.pid!,
      status: 'starting',
      startedAt: Date.now(),
      activeSessions: 0,
      consecutiveFailures: 0,
      backoffMs: BACKOFF_INITIAL_MS,
      restartTimer: null,
      logBuffer: new RingBuffer<string>(1000),
    }

    // Parse worker stdout (NDJSON IPC)
    const rl = createInterface({ input: child.stdout! })
    rl.on('line', (line) => {
      try {
        const msg: WorkerIpcMessage = JSON.parse(line)
        handleWorkerMessage(handle, msg)
      } catch {
        // Not JSON — raw log line
        handle.logBuffer.push(line)
        handle.onLog?.(line)
      }
    })

    // Stderr → log buffer
    const stderrRl = createInterface({ input: child.stderr! })
    stderrRl.on('line', (line) => {
      handle.logBuffer.push(`[stderr] ${line}`)
    })

    // Handle worker exit
    child.on('exit', (code, signal) => {
      registryOpts.log(
        `[registry] worker ${key} exited: code=${code} signal=${signal}`,
      )
      handle.onExit?.(code, signal)

      const elapsed = Date.now() - handle.startedAt
      const fastFailure = elapsed < BACKOFF_FAST_FAILURE_THRESHOLD_MS

      if (code === EXIT_CODE_PERMANENT) {
        // Permanent error — park the worker, don't restart
        handle.status = 'parked'
        registryOpts.log(`[registry] worker ${key} parked (permanent error)`)
        return
      }

      if (handle.status === 'stopping') {
        // Graceful shutdown — don't restart
        workers.delete(key)
        return
      }

      if (fastFailure) {
        handle.consecutiveFailures++
        if (handle.consecutiveFailures >= BACKOFF_MAX_FAST_FAILURES) {
          handle.status = 'parked'
          registryOpts.log(
            `[registry] worker ${key} parked after ${handle.consecutiveFailures} fast failures`,
          )
          return
        }
      } else {
        handle.consecutiveFailures = 0
        handle.backoffMs = BACKOFF_INITIAL_MS
      }

      // Schedule restart with backoff
      handle.status = 'restarting'
      registryOpts.log(
        `[registry] scheduling restart for ${key} in ${handle.backoffMs}ms`,
      )
      handle.restartTimer = setTimeout(() => {
        handle.restartTimer = null
        const newHandle = spawnWorker(opts)
        newHandle.consecutiveFailures = handle.consecutiveFailures
        newHandle.backoffMs = Math.min(
          handle.backoffMs * 2,
          BACKOFF_CAP_MS,
        )
        newHandle.onPermissionRequest = handle.onPermissionRequest
        newHandle.onLog = handle.onLog
        newHandle.onAuth401 = handle.onAuth401
        workers.set(key, newHandle)
      }, handle.backoffMs)
    })

    // Mark as running once we see first log from the worker
    handle.status = 'running'
    workers.set(key, handle)

    // Set up idle monitoring
    resetIdleTimer(key)

    return handle
  }

  function handleWorkerMessage(
    handle: WorkerHandle,
    msg: WorkerIpcMessage,
  ): void {
    switch (msg.type) {
      case 'worker_log': {
        const payload = msg.payload as { message: string } | undefined
        const line = payload?.message ?? JSON.stringify(msg.payload)
        handle.logBuffer.push(line)
        handle.onLog?.(line)
        break
      }
      case 'permission_request': {
        const payload = msg.payload as PermissionRequestPayload
        registryOpts.onPermissionRequest(payload)
        break
      }
      case 'auth_401': {
        const payload = msg.payload as { failedToken: string } | undefined
        registryOpts
          .onAuth401(payload?.failedToken ?? '')
          .then((success) => {
            sendToWorker(handle, {
              type: 'auth_refreshed',
              payload: { success },
            })
          })
        break
      }
      case 'worker_status': {
        const payload = msg.payload as {
          activeSessions?: number
        } | undefined
        if (payload?.activeSessions !== undefined) {
          handle.activeSessions = payload.activeSessions
          // Reset idle timer on activity
          if (handle.activeSessions > 0) {
            clearIdleTimer(handle.key)
          } else {
            resetIdleTimer(handle.key)
          }
        }
        break
      }
    }
  }

  function sendToWorker(handle: WorkerHandle, msg: WorkerIpcMessage): void {
    try {
      if (handle.process.stdin && !handle.process.stdin.destroyed) {
        handle.process.stdin.write(JSON.stringify(msg) + '\n')
      }
    } catch {
      // Worker stdin closed
    }
  }

  function resetIdleTimer(key: string): void {
    clearIdleTimer(key)
    if (registryOpts.idleShutdownMs <= 0) return
    const timer = setTimeout(() => {
      const handle = workers.get(key)
      if (handle && handle.activeSessions === 0) {
        registryOpts.log(`[registry] idle shutdown: ${key}`)
        stopWorker(key, false)
      }
    }, registryOpts.idleShutdownMs)
    timer.unref()
    idleTimers.set(key, timer)
  }

  function clearIdleTimer(key: string): void {
    const timer = idleTimers.get(key)
    if (timer) {
      clearTimeout(timer)
      idleTimers.delete(key)
    }
  }

  async function stopWorker(key: string, force: boolean): Promise<void> {
    const handle = workers.get(key)
    if (!handle) return

    handle.status = 'stopping'
    clearIdleTimer(key)

    if (handle.restartTimer) {
      clearTimeout(handle.restartTimer)
      handle.restartTimer = null
    }

    // Send shutdown command via IPC
    sendToWorker(handle, { type: 'shutdown' })

    // Wait for graceful exit, then force kill
    const graceMs = force ? 0 : 30_000
    await new Promise<void>((resolve) => {
      const forceTimer = setTimeout(() => {
        try {
          handle.process.kill('SIGKILL')
        } catch {
          // Already dead
        }
        resolve()
      }, graceMs)
      forceTimer.unref()

      handle.process.once('exit', () => {
        clearTimeout(forceTimer)
        resolve()
      })

      if (force) {
        try {
          handle.process.kill('SIGKILL')
        } catch {
          // Already dead
        }
      } else {
        try {
          handle.process.kill('SIGTERM')
        } catch {
          // Already dead
        }
      }
    })

    workers.delete(key)
  }

  return {
    getOrSpawn(opts: StartWorkerPayload): WorkerHandle {
      const key = workerKey(opts.dir, opts.spawnMode)
      const existing = workers.get(key)
      if (existing && existing.status !== 'parked') {
        return existing
      }

      if (workers.size >= registryOpts.maxWorkers) {
        throw new Error(
          `Maximum workers (${registryOpts.maxWorkers}) reached. Stop a worker first.`,
        )
      }

      return spawnWorker(opts)
    },

    async stop(key: string, force?: boolean): Promise<void> {
      return stopWorker(key, force ?? false)
    },

    async stopAll(graceMs?: number): Promise<void> {
      const stops = Array.from(workers.keys()).map((key) =>
        stopWorker(key, false),
      )
      await Promise.allSettled(stops)
    },

    listWorkers(): WorkerInfo[] {
      return Array.from(workers.values()).map((h) => ({
        key: h.key,
        dir: h.opts.dir,
        spawnMode: h.opts.spawnMode,
        pid: h.pid,
        status: h.status,
        activeSessions: h.activeSessions,
        capacity: h.opts.capacity,
        uptimeMs: Date.now() - h.startedAt,
        consecutiveFailures: h.consecutiveFailures,
      }))
    },

    get(key: string): WorkerHandle | undefined {
      return workers.get(key)
    },

    has(key: string): boolean {
      return workers.has(key)
    },

    broadcastToken(token: string): void {
      for (const handle of workers.values()) {
        sendToWorker(handle, {
          type: 'token_update',
          payload: { token },
        })
      }
    },

    sendPermissionResolution(
      workerKeyStr: string,
      requestId: string,
      behavior: 'allow' | 'deny',
    ): void {
      const handle = workers.get(workerKeyStr)
      if (handle) {
        sendToWorker(handle, {
          type: 'permission_resolved',
          payload: { requestId, behavior },
        })
      }
    },

    get size(): number {
      return workers.size
    },
  }
}
