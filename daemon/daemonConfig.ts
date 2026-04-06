/**
 * Daemon configuration schema and persistence.
 * Stored at ~/.claude/daemon.json.
 *
 * Uses the same lazySchema + Zod pattern as bridge/bridgePointer.ts.
 */

import { readFile, writeFile, mkdir } from 'fs/promises'
import { dirname, join } from 'path'
import { homedir } from 'os'
import { z } from 'zod/v4'

// ─── Schema ───────────────────────────────────────────────────────────────────

const PersistedWorkerSchema = z.object({
  dir: z.string(),
  spawnMode: z.enum(['same-dir', 'worktree']),
  capacity: z.number().int().positive().default(32),
  sandbox: z.boolean().default(false),
})

const DaemonConfigSchema = z.object({
  version: z.literal(1),
  socketPath: z.string().default(() =>
    join(homedir(), '.claude', 'daemon.sock'),
  ),
  pidPath: z.string().default(() =>
    join(homedir(), '.claude', 'daemon.pid'),
  ),
  logPath: z.string().default(() =>
    join(homedir(), '.claude', 'daemon.log'),
  ),
  maxWorkersPerHost: z.number().int().positive().default(8),
  maxSessionsPerWorker: z.number().int().positive().default(32),
  permissionTtlMs: z.number().int().positive().default(120_000),
  unattendedBehavior: z.enum(['deny', 'notify', 'allow']).default('deny'),
  idleWorkerShutdownMs: z.number().int().positive().default(3_600_000),
  prewarmSessions: z.boolean().default(true),
  persistedWorkers: z.array(PersistedWorkerSchema).default([]),
})

export type DaemonConfig = z.infer<typeof DaemonConfigSchema>
export type PersistedWorker = z.infer<typeof PersistedWorkerSchema>

// ─── Defaults ─────────────────────────────────────────────────────────────────

export function getDefaultDaemonConfig(): DaemonConfig {
  return DaemonConfigSchema.parse({ version: 1 })
}

export function getDaemonConfigPath(): string {
  return join(homedir(), '.claude', 'daemon.json')
}

// ─── Read / Write ─────────────────────────────────────────────────────────────

export async function readDaemonConfig(): Promise<DaemonConfig> {
  const path = getDaemonConfigPath()
  try {
    const raw = await readFile(path, 'utf8')
    return DaemonConfigSchema.parse(JSON.parse(raw))
  } catch {
    return getDefaultDaemonConfig()
  }
}

export async function writeDaemonConfig(config: DaemonConfig): Promise<void> {
  const path = getDaemonConfigPath()
  await mkdir(dirname(path), { recursive: true })
  await writeFile(path, JSON.stringify(config, null, 2), 'utf8')
}

/**
 * Add a worker to the persisted workers list (idempotent by dir + spawnMode).
 */
export async function persistWorker(
  worker: PersistedWorker,
): Promise<DaemonConfig> {
  const config = await readDaemonConfig()
  const existing = config.persistedWorkers.findIndex(
    (w) => w.dir === worker.dir && w.spawnMode === worker.spawnMode,
  )
  if (existing >= 0) {
    config.persistedWorkers[existing] = worker
  } else {
    config.persistedWorkers.push(worker)
  }
  await writeDaemonConfig(config)
  return config
}

/**
 * Remove a worker from the persisted workers list.
 */
export async function unpersistWorker(
  dir: string,
  spawnMode: string,
): Promise<DaemonConfig> {
  const config = await readDaemonConfig()
  config.persistedWorkers = config.persistedWorkers.filter(
    (w) => !(w.dir === dir && w.spawnMode === spawnMode),
  )
  await writeDaemonConfig(config)
  return config
}
