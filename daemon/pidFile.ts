/**
 * Atomic PID file management for the daemon supervisor.
 * Uses tmp + rename for crash-safe writes.
 */

import { readFile, rename, unlink, writeFile } from 'fs/promises'

/**
 * Write the current process PID atomically (write to .tmp, then rename).
 */
export async function writePidFile(path: string): Promise<void> {
  const tmp = path + '.tmp'
  await writeFile(tmp, String(process.pid), 'utf8')
  await rename(tmp, path)
}

/**
 * Read PID from file. Returns null on any failure.
 */
export async function readPidFile(path: string): Promise<number | null> {
  try {
    const content = await readFile(path, 'utf8')
    const pid = parseInt(content.trim(), 10)
    return Number.isFinite(pid) && pid > 0 ? pid : null
  } catch {
    return null
  }
}

/**
 * Check if a process is alive by sending signal 0.
 */
export function isAlive(pid: number): boolean {
  try {
    process.kill(pid, 0)
    return true
  } catch {
    return false
  }
}

/**
 * Remove PID file if the recorded process is no longer alive.
 * Returns true if the file was removed (stale) or didn't exist.
 */
export async function clearStalePidFile(path: string): Promise<boolean> {
  const pid = await readPidFile(path)
  if (pid === null) return true
  if (isAlive(pid)) return false
  try {
    await unlink(path)
  } catch {
    // Already gone
  }
  return true
}

/**
 * Remove PID file unconditionally (used on clean shutdown).
 */
export async function removePidFile(path: string): Promise<void> {
  try {
    await unlink(path)
  } catch {
    // Best-effort
  }
}
