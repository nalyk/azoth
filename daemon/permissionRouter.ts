/**
 * In-memory permission request broker for the daemon supervisor.
 *
 * Routes permission requests from worker sessions to connected IPC clients.
 * Supports multi-client broadcast (first response wins), TTL auto-deny,
 * and "attended" tracking to suppress auto-deny when a human is watching.
 */

import { sendOsNotification } from './osNotification.js'
import type {
  PermissionRequestPayload,
  PermissionResponsePayload,
  DaemonResponse,
} from './ipcProtocol.js'
import type { Connection } from './socketServer.js'

// ─── Types ────────────────────────────────────────────────────────────────────

export type PendingPermission = {
  workerKey: string
  sessionId: string
  request: PermissionRequestPayload
  resolve: (response: PermissionResponsePayload) => void
  reject: (err: Error) => void
  ttlTimer: ReturnType<typeof setTimeout>
  createdAt: number
}

export type PermissionRouterOpts = {
  /** Default TTL for unattended permission requests (ms). */
  defaultTtlMs: number
  /** What to do when no client responds before TTL. */
  unattendedBehavior: 'deny' | 'notify' | 'allow'
  /** Callback to broadcast to all connected IPC clients. */
  broadcastToClients: (msg: DaemonResponse) => void
}

export type PermissionRouter = {
  /**
   * Add a pending permission request. Returns a Promise that resolves
   * when any IPC client responds (or TTL expires).
   */
  addPendingRequest(
    request: PermissionRequestPayload,
    ttlMs?: number,
  ): Promise<PermissionResponsePayload>

  /**
   * Resolve a pending request (called when a client sends permission_response).
   * Returns false if requestId is unknown or expired.
   */
  resolveRequest(
    requestId: string,
    response: PermissionResponsePayload,
  ): boolean

  /** Get all pending requests (for late-attaching clients). */
  getPendingRequests(): PermissionRequestPayload[]

  /** Get count of pending requests. */
  pendingCount(): number

  /**
   * Mark a worker as "attended" (a client is actively watching its logs).
   * Suppresses TTL auto-deny for that worker.
   */
  markAttended(workerKey: string): void

  /** Mark a worker as "unattended". */
  markUnattended(workerKey: string): void

  /** Clean up all pending requests (on shutdown). */
  clear(): void
}

// ─── Implementation ───────────────────────────────────────────────────────────

export function createPermissionRouter(
  opts: PermissionRouterOpts,
): PermissionRouter {
  const pending = new Map<string, PendingPermission>()
  const attendedWorkers = new Set<string>()

  function resolvePending(
    requestId: string,
    response: PermissionResponsePayload,
  ): boolean {
    const entry = pending.get(requestId)
    if (!entry) return false

    clearTimeout(entry.ttlTimer)
    pending.delete(requestId)
    entry.resolve(response)

    // Broadcast resolution to all clients so they can update UI
    opts.broadcastToClients({
      id: requestId,
      type: 'permission_resolved',
      payload: response,
    })

    return true
  }

  return {
    addPendingRequest(
      request: PermissionRequestPayload,
      ttlMs?: number,
    ): Promise<PermissionResponsePayload> {
      return new Promise((resolve, reject) => {
        const effectiveTtl = ttlMs ?? opts.defaultTtlMs
        const isAttended = attendedWorkers.has(request.workerKey)

        // Set up TTL timer (suppressed if attended)
        const ttlTimer = isAttended
          ? setTimeout(() => {}, 0x7fffffff) // Never fires (max safe timeout)
          : setTimeout(() => {
              const entry = pending.get(request.requestId)
              if (!entry) return

              pending.delete(request.requestId)

              // Apply unattended behavior
              if (opts.unattendedBehavior === 'notify') {
                sendOsNotification(
                  'Claude Code Permission Request',
                  `${request.toolName}: ${request.description}`,
                )
              }

              const behavior =
                opts.unattendedBehavior === 'allow' ? 'allow' : 'deny'

              const response: PermissionResponsePayload = {
                requestId: request.requestId,
                behavior,
              }
              entry.resolve(response)

              opts.broadcastToClients({
                id: request.requestId,
                type: 'permission_resolved',
                payload: { ...response, reason: 'ttl_expired' },
              })
            }, effectiveTtl)

        // Store pending entry
        pending.set(request.requestId, {
          workerKey: request.workerKey,
          sessionId: request.sessionId,
          request,
          resolve,
          reject,
          ttlTimer,
          createdAt: Date.now(),
        })

        // Broadcast to all connected clients
        opts.broadcastToClients({
          id: request.requestId,
          type: 'permission_request',
          payload: request,
        })
      })
    },

    resolveRequest(
      requestId: string,
      response: PermissionResponsePayload,
    ): boolean {
      return resolvePending(requestId, response)
    },

    getPendingRequests(): PermissionRequestPayload[] {
      return Array.from(pending.values()).map((p) => p.request)
    },

    pendingCount(): number {
      return pending.size
    },

    markAttended(workerKey: string): void {
      attendedWorkers.add(workerKey)
    },

    markUnattended(workerKey: string): void {
      attendedWorkers.delete(workerKey)
    },

    clear(): void {
      for (const entry of pending.values()) {
        clearTimeout(entry.ttlTimer)
        entry.reject(new Error('Permission router shutting down'))
      }
      pending.clear()
      attendedWorkers.clear()
    },
  }
}
