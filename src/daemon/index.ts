/**
 * Claude Code Daemon Mode
 *
 * A background process architecture that eliminates cold-start latency
 * by maintaining persistent worker processes with warm OAuth tokens,
 * pre-registered environments, and session pre-warming.
 *
 * Architecture:
 *
 *   CLI Client  ──(Unix socket)──>  Supervisor  ──(stdio pipes)──>  Workers
 *   (ephemeral)                     (long-lived)                    (per-dir)
 *                                       │
 *                                       ├── AuthManager (OAuth lifecycle)
 *                                       ├── PermissionRouter (TTL broker)
 *                                       ├── WorkerRegistry (process pool)
 *                                       └── ConversationCache (LRU)
 *
 * Each worker calls the existing runBridgeHeadless() from bridge/bridgeMain.ts,
 * which was explicitly designed for daemon callers (see comments at line 2800).
 *
 * Entry points:
 *   claude --daemon-supervisor  → supervisor.ts (internal)
 *   claude --daemon-worker      → worker.ts (internal)
 *   claude daemon <subcommand>  → client.ts (user-facing)
 */

// Core utilities
export { RingBuffer } from './ringBuffer.js'
export {
  frameEncode,
  FrameDecoder,
  nextRequestId,
  workerKey,
  EXIT_CODE_PERMANENT,
  EXIT_CODE_OK,
  EXIT_CODE_TRANSIENT,
} from './ipcProtocol.js'

// Types
export type {
  DaemonRequest,
  DaemonResponse,
  DaemonRequestType,
  DaemonResponseType,
  WorkerIpcMessage,
  WorkerIpcType,
  StartWorkerPayload,
  StopWorkerPayload,
  PermissionRequestPayload,
  PermissionResponsePayload,
  WorkerInfo,
  WorkerStatus,
  StatusResponse,
  SubscribeLogsPayload,
} from './ipcProtocol.js'

// Socket
export { createSocketServer, connectToSocket } from './socketServer.js'
export type { Connection, DaemonSocketServer, DaemonSocketClient } from './socketServer.js'

// Config
export {
  readDaemonConfig,
  writeDaemonConfig,
  getDefaultDaemonConfig,
  getDaemonConfigPath,
  persistWorker,
  unpersistWorker,
} from './daemonConfig.js'
export type { DaemonConfig, PersistedWorker } from './daemonConfig.js'

// PID management
export { writePidFile, readPidFile, isAlive, clearStalePidFile, removePidFile } from './pidFile.js'

// Auth
export { createAuthManager } from './authManager.js'
export type { AuthManager } from './authManager.js'

// Permissions
export { createPermissionRouter } from './permissionRouter.js'
export type { PermissionRouter, PendingPermission, PermissionRouterOpts } from './permissionRouter.js'

// Workers
export { createWorkerRegistry } from './workerRegistry.js'
export type { WorkerHandle, WorkerRegistry, WorkerRegistryOpts } from './workerRegistry.js'

// Conversation cache
export { createConversationCache, parseLogForMessage } from './conversationCache.js'
export type { ConversationCache, CachedMessage } from './conversationCache.js'

// OS notifications
export { sendOsNotification } from './osNotification.js'

// Banner and UI
export {
  renderBanner,
  renderSupervisorBanner,
  renderStatusHeader,
  formatWorkerRow,
  workerTableHeader,
  formatPermissionRequest,
  logPrefix,
} from './banner.js'

// Entry points (imported for side effects when --daemon-supervisor or --daemon-worker flags present)
// import './supervisor.js'  // claude --daemon-supervisor
// import './worker.js'      // claude --daemon-worker
// import './client.js'      // claude daemon <subcommand>
