/**
 * Daemon worker process entrypoint.
 *
 * Invoked via `claude --daemon-worker`. Reads HeadlessBridgeOpts from stdin
 * as a single JSON line, then calls the existing runBridgeHeadless() with
 * IPC relay wired between child sessions and the supervisor.
 *
 * Communication with supervisor: stdio pipes (NDJSON on stdout, JSON on stdin).
 * Communication with child sessions: inherited from runBridgeHeadless() via
 * the existing bridge/sessionRunner.ts subprocess spawning.
 *
 * Exit codes:
 *   0  — clean shutdown (supervisor signal or AbortController)
 *  78  — permanent error (EXIT_CODE_PERMANENT) — supervisor should park
 *   1  — transient error — supervisor should backoff-retry
 */

import { createInterface } from 'readline'
import {
  type WorkerIpcMessage,
  type PermissionRequestPayload,
  EXIT_CODE_PERMANENT,
  EXIT_CODE_OK,
  EXIT_CODE_TRANSIENT,
} from './ipcProtocol.js'

// ─── IPC helpers ──────────────────────────────────────────────────────────────

function sendToSupervisor(msg: WorkerIpcMessage): void {
  try {
    process.stdout.write(JSON.stringify(msg) + '\n')
  } catch {
    // stdout closed — supervisor died, we'll detect via EOF
  }
}

function log(s: string): void {
  sendToSupervisor({ type: 'worker_log', payload: { message: s } })
}

// ─── Main ─────────────────────────────────────────────────────────────────────

export async function runDaemonWorker(): Promise<never> {
  let exitCode = EXIT_CODE_OK

  try {
    // 1. Read configuration from stdin (single JSON line from supervisor)
    const config = await readConfigFromStdin()
    log(`[worker] starting for dir=${config.dir} mode=${config.spawnMode}`)

    // 2. Set up abort controller for graceful shutdown
    const abort = new AbortController()

    // Supervisor pipe EOF → abort (supervisor died)
    process.stdin.on('end', () => {
      log('[worker] supervisor pipe EOF — shutting down')
      abort.abort()
    })

    // SIGTERM from supervisor → abort
    process.on('SIGTERM', () => {
      log('[worker] received SIGTERM — shutting down')
      abort.abort()
    })

    // 3. Set up token management via supervisor IPC
    let currentToken: string | undefined = config.initialToken

    // Listen for supervisor IPC messages on stdin
    const stdinRl = createInterface({ input: process.stdin })
    stdinRl.on('line', (line) => {
      try {
        const msg: WorkerIpcMessage = JSON.parse(line)
        handleSupervisorMessage(msg)
      } catch {
        // Not JSON — ignore
      }
    })

    // Pending auth refresh resolvers (dedup concurrent 401s)
    let pendingAuthResolve: ((ok: boolean) => void) | null = null

    function handleSupervisorMessage(msg: WorkerIpcMessage): void {
      switch (msg.type) {
        case 'token_update': {
          const payload = msg.payload as { token: string } | undefined
          if (payload?.token) {
            currentToken = payload.token
            log('[worker] token updated from supervisor')
          }
          break
        }
        case 'auth_refreshed': {
          const payload = msg.payload as { success: boolean } | undefined
          if (pendingAuthResolve) {
            pendingAuthResolve(payload?.success ?? false)
            pendingAuthResolve = null
          }
          break
        }
        case 'permission_resolved': {
          // Forward permission response to child sessions
          // This is handled by the bridge loop internally via the
          // onPermissionRequest callback wiring
          break
        }
        case 'shutdown': {
          abort.abort()
          break
        }
      }
    }

    // 4. Build HeadlessBridgeOpts with supervisor-backed callbacks
    const {
      runBridgeHeadless,
      BridgeHeadlessPermanentError,
    } = await import('../bridge/bridgeMain.js')

    const opts = {
      dir: config.dir,
      name: config.name,
      spawnMode: config.spawnMode as 'same-dir' | 'worktree',
      capacity: config.capacity ?? 32,
      permissionMode: config.permissionMode,
      sandbox: config.sandbox ?? false,
      sessionTimeoutMs: config.sessionTimeoutMs,
      createSessionOnStart: config.prewarm ?? false,

      getAccessToken(): string | undefined {
        return currentToken
      },

      async onAuth401(failedToken: string): Promise<boolean> {
        // Dedup: if already refreshing, piggyback
        if (pendingAuthResolve) {
          return new Promise<boolean>((resolve) => {
            const prev = pendingAuthResolve!
            pendingAuthResolve = (ok) => {
              prev(ok)
              resolve(ok)
            }
          })
        }
        // Ask supervisor to refresh
        sendToSupervisor({
          type: 'auth_401',
          payload: { failedToken },
        })
        return new Promise<boolean>((resolve) => {
          pendingAuthResolve = resolve
          // Timeout: if supervisor doesn't respond in 30s, give up
          setTimeout(() => {
            if (pendingAuthResolve === resolve) {
              pendingAuthResolve = null
              resolve(false)
            }
          }, 30_000)
        })
      },

      log,

      onPermissionRequest(
        sessionId: string,
        request: unknown,
        accessToken: string,
      ): void {
        const req = request as Record<string, unknown> | undefined
        const payload: PermissionRequestPayload = {
          requestId: (req?.request_id as string) ?? `perm-${Date.now()}`,
          workerKey: `${config.dir}:${config.spawnMode}`,
          sessionId,
          toolName: ((req?.request as any)?.tool_name as string) ?? 'unknown',
          toolUseId: ((req?.request as any)?.tool_use_id as string) ?? '',
          description: `Tool use: ${((req?.request as any)?.tool_name as string) ?? 'unknown'}`,
          input: ((req?.request as any)?.input as Record<string, unknown>) ?? {},
          autoApproveAfterMs: 0,
        }
        sendToSupervisor({ type: 'permission_request', payload })
      },
    }

    // 5. Run the headless bridge loop
    try {
      await runBridgeHeadless(opts, abort.signal)
      exitCode = EXIT_CODE_OK
    } catch (err: unknown) {
      if (err instanceof BridgeHeadlessPermanentError) {
        log(`[worker] permanent error: ${err.message}`)
        exitCode = EXIT_CODE_PERMANENT
      } else {
        log(`[worker] transient error: ${(err as Error)?.message ?? err}`)
        exitCode = EXIT_CODE_TRANSIENT
      }
    }

    stdinRl.close()
  } catch (err: unknown) {
    log(`[worker] fatal startup error: ${(err as Error)?.message ?? err}`)
    exitCode = EXIT_CODE_TRANSIENT
  }

  process.exit(exitCode)
}

// ─── Config reader ────────────────────────────────────────────────────────────

type WorkerConfig = {
  dir: string
  name?: string
  spawnMode: string
  capacity?: number
  permissionMode?: string
  sandbox?: boolean
  sessionTimeoutMs?: number
  prewarm?: boolean
  initialToken?: string
}

async function readConfigFromStdin(): Promise<WorkerConfig> {
  return new Promise((resolve, reject) => {
    let data = ''
    const timeout = setTimeout(() => {
      reject(new Error('Timeout waiting for config on stdin'))
    }, 10_000)

    const rl = createInterface({ input: process.stdin })
    rl.once('line', (line) => {
      clearTimeout(timeout)
      rl.close()
      try {
        resolve(JSON.parse(line) as WorkerConfig)
      } catch (err) {
        reject(new Error(`Invalid config JSON: ${err}`))
      }
    })
    rl.once('close', () => {
      clearTimeout(timeout)
      if (!data) reject(new Error('stdin closed before config received'))
    })
  })
}

// ─── Entry point ──────────────────────────────────────────────────────────────

// When this module is the entrypoint:
if (process.argv.includes('--daemon-worker')) {
  runDaemonWorker()
}
