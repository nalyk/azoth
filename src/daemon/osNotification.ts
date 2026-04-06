/**
 * Platform-specific OS notification dispatch.
 * Fire-and-forget — gracefully no-ops if the command is unavailable.
 */

import { spawn } from 'child_process'
import { platform } from 'os'

/**
 * Send a desktop notification. Non-blocking, never throws.
 */
export function sendOsNotification(title: string, body: string): void {
  try {
    const os = platform()
    if (os === 'linux') {
      // notify-send is standard on most Linux desktops
      const child = spawn('notify-send', [title, body], {
        stdio: 'ignore',
        detached: true,
      })
      child.unref()
    } else if (os === 'darwin') {
      const script = `display notification "${escapeAppleScript(body)}" with title "${escapeAppleScript(title)}"`
      const child = spawn('osascript', ['-e', script], {
        stdio: 'ignore',
        detached: true,
      })
      child.unref()
    }
    // Windows: not supported yet (could use PowerShell toast in the future)
  } catch {
    // Swallow — notifications are best-effort
  }
}

function escapeAppleScript(s: string): string {
  return s.replace(/\\/g, '\\\\').replace(/"/g, '\\"')
}
