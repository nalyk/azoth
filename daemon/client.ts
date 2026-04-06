/**
 * Thin CLI client for the Claude Code daemon.
 *
 * Dispatched via `claude daemon <subcommand>`. Connects to the supervisor's
 * Unix domain socket, sends a request, renders the response, and exits.
 *
 * Subcommands:
 *   start [dir]           — Start a daemon worker for a directory
 *   stop [dir]            — Stop a daemon worker
 *   status                — Show daemon status and all workers
 *   logs [dir]            — Stream worker logs (Ctrl+C to stop)
 *   approve <requestId>   — Approve a pending permission request
 *   deny <requestId>      — Deny a pending permission request
 *   install-service       — Install systemd/launchd service file
 */

import { resolve } from 'path'
import { homedir } from 'os'
import { spawn } from 'child_process'
import { access, copyFile, mkdir } from 'fs/promises'
import { connectToSocket, type DaemonSocketClient } from './socketServer.js'
import {
  type DaemonRequest,
  type DaemonResponse,
  type WorkerInfo,
  type StatusResponse,
  type PermissionRequestPayload,
  nextRequestId,
  workerKey,
} from './ipcProtocol.js'
import { getDaemonConfigPath, readDaemonConfig } from './daemonConfig.js'
import { readPidFile, isAlive } from './pidFile.js'

// ─── Main dispatcher ──────────────────────────────────────────────────────────

export async function runDaemonClient(args: string[]): Promise<void> {
  const subcommand = args[0]

  switch (subcommand) {
    case 'start':
      await cmdStart(args.slice(1))
      break
    case 'stop':
      await cmdStop(args.slice(1))
      break
    case 'status':
      await cmdStatus()
      break
    case 'logs':
      await cmdLogs(args.slice(1))
      break
    case 'approve':
      await cmdPermission(args[1], 'allow')
      break
    case 'deny':
      await cmdPermission(args[1], 'deny')
      break
    case 'install-service':
      await cmdInstallService()
      break
    default:
      printUsage()
      process.exit(subcommand ? 1 : 0)
  }
}

// ─── Subcommands ──────────────────────────────────────────────────────────────

async function cmdStart(args: string[]): Promise<void> {
  const dir = resolve(args[0] || process.cwd())
  const spawnMode = args.includes('--worktree') ? 'worktree' : 'same-dir'
  const force = args.includes('--force')

  const client = await ensureDaemonRunning()

  const response = await sendRequest(client, {
    id: nextRequestId(),
    type: 'start_worker',
    payload: {
      dir,
      spawnMode,
      capacity: 32,
      sandbox: false,
      prewarm: true,
    },
  })

  if (response.type === 'worker_started') {
    const info = response.payload as WorkerInfo
    console.log(`Worker started: ${info.key}`)
    console.log(`  PID: ${info.pid}`)
    console.log(`  Status: ${info.status}`)
    console.log(`  Capacity: ${info.capacity} sessions`)
  } else if (response.type === 'error') {
    const err = response.payload as { message: string }
    console.error(`Error: ${err.message}`)
    process.exit(1)
  }

  client.close()
}

async function cmdStop(args: string[]): Promise<void> {
  const dir = resolve(args[0] || process.cwd())
  const spawnMode = args.includes('--worktree') ? 'worktree' : 'same-dir'
  const force = args.includes('--force')

  const client = await connectToDaemon()

  const response = await sendRequest(client, {
    id: nextRequestId(),
    type: 'stop_worker',
    payload: { dir, spawnMode, force },
  })

  if (response.type === 'worker_stopped') {
    const payload = response.payload as { key: string }
    console.log(`Worker stopped: ${payload.key}`)
  } else if (response.type === 'error') {
    const err = response.payload as { message: string }
    console.error(`Error: ${err.message}`)
    process.exit(1)
  }

  client.close()
}

async function cmdStatus(): Promise<void> {
  const client = await connectToDaemon()

  const response = await sendRequest(client, {
    id: nextRequestId(),
    type: 'status',
  })

  if (response.type === 'status_response') {
    const status = response.payload as StatusResponse
    console.log(`Claude Code Daemon`)
    console.log(`  Version: ${status.version}`)
    console.log(`  Uptime: ${formatDuration(status.uptime)}`)
    console.log(`  Pending permissions: ${status.pendingPermissions}`)
    console.log()

    if (status.workers.length === 0) {
      console.log('  No active workers.')
    } else {
      console.log('  Workers:')
      console.log(
        '  ' +
          'KEY'.padEnd(50) +
          'PID'.padEnd(8) +
          'STATUS'.padEnd(12) +
          'SESSIONS'.padEnd(10) +
          'UPTIME',
      )
      console.log('  ' + '-'.repeat(90))
      for (const w of status.workers) {
        console.log(
          '  ' +
            w.key.padEnd(50) +
            String(w.pid).padEnd(8) +
            w.status.padEnd(12) +
            `${w.activeSessions}/${w.capacity}`.padEnd(10) +
            formatDuration(w.uptimeMs),
        )
      }
    }
  } else if (response.type === 'error') {
    const err = response.payload as { message: string }
    console.error(`Error: ${err.message}`)
    process.exit(1)
  }

  client.close()
}

async function cmdLogs(args: string[]): Promise<void> {
  const dir = resolve(args[0] || process.cwd())
  const spawnMode = args.includes('--worktree') ? 'worktree' : 'same-dir'
  const key = workerKey(dir, spawnMode)

  const client = await connectToDaemon()

  // Subscribe to logs
  const subResponse = await sendRequest(client, {
    id: nextRequestId(),
    type: 'subscribe_worker_logs',
    payload: { workerKey: key },
  })

  if (subResponse.type === 'error') {
    const err = subResponse.payload as { message: string }
    console.error(`Error: ${err.message}`)
    process.exit(1)
  }

  console.log(`Streaming logs for ${key} (Ctrl+C to stop)...`)
  console.log()

  // Listen for log and permission messages
  client.onMessage((raw) => {
    const msg = raw as DaemonResponse
    if (msg.type === 'worker_log') {
      const payload = msg.payload as { workerKey: string; message: string }
      process.stdout.write(payload.message + '\n')
    } else if (msg.type === 'permission_request') {
      const payload = msg.payload as PermissionRequestPayload
      console.log()
      console.log(`--- Permission Request ---`)
      console.log(`  Tool: ${payload.toolName}`)
      console.log(`  Description: ${payload.description}`)
      console.log(`  Request ID: ${payload.requestId}`)
      console.log(
        `  Run: claude daemon approve ${payload.requestId}`,
      )
      console.log(
        `  Or:  claude daemon deny ${payload.requestId}`,
      )
      console.log()
    } else if (msg.type === 'permission_resolved') {
      const payload = msg.payload as { requestId: string; behavior: string }
      console.log(`[permission ${payload.requestId}] → ${payload.behavior}`)
    }
  })

  // Wait for Ctrl+C
  await new Promise<void>((resolve) => {
    process.on('SIGINT', () => {
      console.log('\nDisconnecting...')
      client.close()
      resolve()
    })
    client.onClose(() => resolve())
  })
}

async function cmdPermission(
  requestId: string | undefined,
  behavior: 'allow' | 'deny',
): Promise<void> {
  if (!requestId) {
    console.error(`Usage: claude daemon ${behavior === 'allow' ? 'approve' : 'deny'} <requestId>`)
    process.exit(1)
  }

  const client = await connectToDaemon()

  const response = await sendRequest(client, {
    id: nextRequestId(),
    type: 'permission_response',
    payload: { requestId, behavior },
  })

  if (response.type === 'ok') {
    console.log(`Permission ${behavior === 'allow' ? 'approved' : 'denied'}: ${requestId}`)
  } else if (response.type === 'error') {
    const err = response.payload as { message: string }
    console.error(`Error: ${err.message}`)
    process.exit(1)
  }

  client.close()
}

async function cmdInstallService(): Promise<void> {
  const platform = process.platform

  if (platform === 'linux') {
    const serviceDir = resolve(homedir(), '.config/systemd/user')
    const servicePath = resolve(serviceDir, 'claude-daemon.service')
    const srcPath = resolve(__dirname, '../contrib/systemd/claude-daemon.service')

    await mkdir(serviceDir, { recursive: true })
    await copyFile(srcPath, servicePath)
    console.log(`Installed: ${servicePath}`)
    console.log('Run: systemctl --user daemon-reload && systemctl --user enable --now claude-daemon')
  } else if (platform === 'darwin') {
    const launchAgentsDir = resolve(homedir(), 'Library/LaunchAgents')
    const plistPath = resolve(launchAgentsDir, 'com.anthropic.claude-daemon.plist')
    const srcPath = resolve(__dirname, '../contrib/launchd/com.anthropic.claude-daemon.plist')

    await mkdir(launchAgentsDir, { recursive: true })
    await copyFile(srcPath, plistPath)
    console.log(`Installed: ${plistPath}`)
    console.log('Run: launchctl load ~/Library/LaunchAgents/com.anthropic.claude-daemon.plist')
  } else {
    console.error(`Platform ${platform} is not supported for service installation.`)
    process.exit(1)
  }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

function printUsage(): void {
  console.log(`Usage: claude daemon <subcommand> [options]

Subcommands:
  start [dir]           Start a daemon worker for a directory
  stop [dir]            Stop a daemon worker
  status                Show daemon status and all workers
  logs [dir]            Stream worker logs (Ctrl+C to stop)
  approve <requestId>   Approve a pending permission request
  deny <requestId>      Deny a pending permission request
  install-service       Install systemd/launchd service file

Options:
  --worktree            Use worktree spawn mode (default: same-dir)
  --force               Force stop (kill immediately)`)
}

async function connectToDaemon(): Promise<DaemonSocketClient> {
  const config = await readDaemonConfig()
  const client = connectToSocket(config.socketPath)

  try {
    await client.connected
    return client
  } catch {
    console.error('Cannot connect to daemon. Is it running?')
    console.error('Start with: claude --daemon-supervisor &')
    process.exit(1)
  }
}

async function ensureDaemonRunning(): Promise<DaemonSocketClient> {
  const config = await readDaemonConfig()

  // Try connecting first
  try {
    const client = connectToSocket(config.socketPath)
    await client.connected
    return client
  } catch {
    // Daemon not running — auto-start it
  }

  console.log('Starting daemon supervisor...')

  // Check for stale PID
  const pid = await readPidFile(config.pidPath)
  if (pid !== null && isAlive(pid)) {
    // Daemon running but socket unreachable — wait a bit
    await sleep(1000)
    try {
      const client = connectToSocket(config.socketPath)
      await client.connected
      return client
    } catch {
      console.error('Daemon PID exists but socket unreachable.')
      process.exit(1)
    }
  }

  // Spawn supervisor in background
  const child = spawn(process.execPath, [...process.argv.slice(1, -1), '--daemon-supervisor'], {
    detached: true,
    stdio: 'ignore',
  })
  child.unref()

  // Poll for socket availability (up to 5s)
  for (let i = 0; i < 50; i++) {
    await sleep(100)
    try {
      const client = connectToSocket(config.socketPath)
      await client.connected
      console.log('Daemon started.')
      return client
    } catch {
      // Not ready yet
    }
  }

  console.error('Timeout waiting for daemon to start.')
  process.exit(1)
}

function sendRequest(
  client: DaemonSocketClient,
  request: DaemonRequest,
): Promise<DaemonResponse> {
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      reject(new Error('Request timeout (10s)'))
    }, 10_000)

    client.onMessage((raw) => {
      const msg = raw as DaemonResponse
      if (msg.id === request.id) {
        clearTimeout(timeout)
        resolve(msg)
      }
    })

    client.send(request)
  })
}

function formatDuration(ms: number): string {
  const seconds = Math.floor(ms / 1000)
  const minutes = Math.floor(seconds / 60)
  const hours = Math.floor(minutes / 60)
  const days = Math.floor(hours / 24)

  if (days > 0) return `${days}d ${hours % 24}h`
  if (hours > 0) return `${hours}h ${minutes % 60}m`
  if (minutes > 0) return `${minutes}m ${seconds % 60}s`
  return `${seconds}s`
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms))
}

// ─── Entry point ──────────────────────────────────────────────────────────────

// When invoked as `claude daemon <subcommand>`
const daemonIdx = process.argv.indexOf('daemon')
if (daemonIdx >= 0) {
  runDaemonClient(process.argv.slice(daemonIdx + 1))
}
