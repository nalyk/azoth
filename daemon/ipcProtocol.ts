/**
 * IPC protocol for daemon supervisor ↔ client and supervisor ↔ worker
 * communication over Unix domain sockets and stdio pipes.
 *
 * Frame format: 4-byte big-endian length prefix + UTF-8 JSON payload.
 * Matches the NDJSON pattern used in bridge/sessionRunner.ts.
 */

// ─── Request types (client → supervisor) ──────────────────────────────────────

export type DaemonRequestType =
  | 'ping'
  | 'status'
  | 'start_worker'
  | 'stop_worker'
  | 'list_workers'
  | 'permission_response'
  | 'subscribe_worker_logs'
  | 'unsubscribe_worker_logs'
  | 'upgrade_check'

// ─── Response types (supervisor → client) ─────────────────────────────────────

export type DaemonResponseType =
  | 'pong'
  | 'status_response'
  | 'worker_started'
  | 'worker_stopped'
  | 'worker_list'
  | 'permission_request'
  | 'permission_resolved'
  | 'worker_log'
  | 'error'
  | 'ok'

// ─── Worker IPC types (supervisor ↔ worker over stdio) ────────────────────────

export type WorkerIpcType =
  | 'worker_log'
  | 'permission_request'
  | 'permission_resolved'
  | 'token_update'
  | 'auth_401'
  | 'auth_refreshed'
  | 'prewarm_session'
  | 'worker_status'
  | 'shutdown'

// ─── Core message envelope ────────────────────────────────────────────────────

export type DaemonRequest = {
  id: string
  type: DaemonRequestType
  payload?: unknown
}

export type DaemonResponse = {
  id: string
  type: DaemonResponseType
  payload?: unknown
}

export type WorkerIpcMessage = {
  type: WorkerIpcType
  payload?: unknown
}

// ─── Payload types ────────────────────────────────────────────────────────────

export type StartWorkerPayload = {
  dir: string
  spawnMode: 'same-dir' | 'worktree'
  capacity: number
  sandbox: boolean
  sessionTimeoutMs?: number
  prewarm: boolean
  permissionMode?: string
}

export type StopWorkerPayload = {
  dir: string
  spawnMode?: 'same-dir' | 'worktree'
  force?: boolean
}

export type PermissionRequestPayload = {
  requestId: string
  workerKey: string
  sessionId: string
  toolName: string
  toolUseId: string
  description: string
  input: Record<string, unknown>
  autoApproveAfterMs: number
}

export type PermissionResponsePayload = {
  requestId: string
  behavior: 'allow' | 'deny'
}

export type WorkerInfo = {
  key: string
  dir: string
  spawnMode: 'same-dir' | 'worktree'
  pid: number
  status: WorkerStatus
  activeSessions: number
  capacity: number
  uptimeMs: number
  consecutiveFailures: number
}

export type WorkerStatus =
  | 'starting'
  | 'running'
  | 'parked'
  | 'restarting'
  | 'stopping'

export type StatusResponse = {
  version: string
  uptime: number
  workers: WorkerInfo[]
  pendingPermissions: number
}

export type SubscribeLogsPayload = {
  workerKey: string
}

// ─── Frame encoding/decoding ──────────────────────────────────────────────────

/**
 * Encode a message as a length-prefixed frame.
 * Format: [4 bytes big-endian length][UTF-8 JSON payload]
 */
export function frameEncode(msg: unknown): Buffer {
  const json = JSON.stringify(msg)
  const payload = Buffer.from(json, 'utf8')
  const frame = Buffer.allocUnsafe(4 + payload.length)
  frame.writeUInt32BE(payload.length, 0)
  payload.copy(frame, 4)
  return frame
}

/**
 * Stateful frame decoder. Feed it raw socket data via `push()`,
 * and it calls `onMessage` for each complete frame decoded.
 */
export class FrameDecoder {
  private buffer = Buffer.alloc(0)
  private onMessage: (msg: unknown) => void

  constructor(onMessage: (msg: unknown) => void) {
    this.onMessage = onMessage
  }

  push(data: Buffer): void {
    this.buffer = Buffer.concat([this.buffer, data])
    this.drain()
  }

  private drain(): void {
    while (this.buffer.length >= 4) {
      const len = this.buffer.readUInt32BE(0)
      if (this.buffer.length < 4 + len) break
      const payload = this.buffer.subarray(4, 4 + len)
      this.buffer = this.buffer.subarray(4 + len)
      try {
        this.onMessage(JSON.parse(payload.toString('utf8')))
      } catch {
        // Malformed JSON — skip frame
      }
    }
  }

  reset(): void {
    this.buffer = Buffer.alloc(0)
  }
}

// ─── Helper: generate request IDs ────────────────────────────────────────────

let counter = 0
export function nextRequestId(): string {
  return `req-${Date.now()}-${++counter}`
}

// ─── Worker key generation ────────────────────────────────────────────────────

export function workerKey(dir: string, spawnMode: string): string {
  return `${dir}:${spawnMode}`
}

// ─── Exit codes for worker → supervisor communication ─────────────────────────

/** Worker exited due to a permanent configuration error — do NOT restart. */
export const EXIT_CODE_PERMANENT = 78 // EX_CONFIG from sysexits.h

/** Worker exited normally (graceful shutdown). */
export const EXIT_CODE_OK = 0

/** Worker exited due to a transient error — supervisor should backoff-retry. */
export const EXIT_CODE_TRANSIENT = 1
