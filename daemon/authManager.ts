/**
 * Single-process OAuth token lifecycle manager for the daemon supervisor.
 *
 * Wraps the existing OAuth utilities with an in-memory cache so worker
 * processes never read tokens from disk. Broadcasts token updates to
 * all subscribers (worker pipes) on refresh.
 *
 * Reuses createTokenRefreshScheduler() from bridge/jwtUtils.ts.
 */

// ─── Types ────────────────────────────────────────────────────────────────────

export type AuthManager = {
  /** Get the current cached access token (no disk I/O). */
  getAccessToken(): string | undefined

  /**
   * Handle a 401 from a worker. If staleToken matches current, triggers refresh.
   * Deduplicates concurrent calls. Returns true if token was refreshed.
   */
  onAuth401(staleToken: string): Promise<boolean>

  /** Subscribe to token updates. Returns unsubscribe function. */
  subscribeToTokenUpdates(cb: (token: string) => void): () => void

  /** Start background token refresh loop. */
  startRefreshLoop(): void

  /** Stop background refresh and clean up. */
  stopRefreshLoop(): void

  /** Force an immediate token refresh. */
  forceRefresh(): Promise<boolean>
}

// ─── Implementation ───────────────────────────────────────────────────────────

export function createAuthManager(): AuthManager {
  let currentToken: string | undefined
  let refreshTimer: ReturnType<typeof setInterval> | null = null
  const subscribers = new Set<(token: string) => void>()

  // Dedup concurrent 401 handling
  let activeRefresh: Promise<boolean> | null = null

  function broadcastToken(token: string): void {
    for (const cb of subscribers) {
      try {
        cb(token)
      } catch {
        // Subscriber error — don't break broadcast
      }
    }
  }

  async function doRefresh(): Promise<boolean> {
    try {
      // Dynamically import to avoid pulling bridge/utils into supervisor at module level.
      // These are loaded lazily so the supervisor's import graph stays clean.
      const { getClaudeAIOAuthTokens } = await import(
        '../utils/auth.js'
      )

      const tokens = await getClaudeAIOAuthTokens()
      if (tokens?.accessToken && tokens.accessToken !== currentToken) {
        currentToken = tokens.accessToken
        broadcastToken(currentToken)
        return true
      }
      return !!tokens?.accessToken
    } catch {
      return false
    }
  }

  return {
    getAccessToken(): string | undefined {
      return currentToken
    },

    async onAuth401(staleToken: string): Promise<boolean> {
      // If token already updated by another refresh, no-op
      if (currentToken && currentToken !== staleToken) {
        return true
      }

      // Dedup: piggyback on active refresh
      if (activeRefresh) {
        return activeRefresh
      }

      activeRefresh = doRefresh().finally(() => {
        activeRefresh = null
      })

      return activeRefresh
    },

    subscribeToTokenUpdates(cb: (token: string) => void): () => void {
      subscribers.add(cb)
      // Send current token immediately if available
      if (currentToken) {
        try {
          cb(currentToken)
        } catch {
          // ignore
        }
      }
      return () => subscribers.delete(cb)
    },

    startRefreshLoop(): void {
      // Initial load
      doRefresh()

      // Refresh every 5 minutes (well before typical JWT expiry)
      refreshTimer = setInterval(() => {
        doRefresh()
      }, 5 * 60 * 1000)
      refreshTimer.unref() // Don't prevent process exit
    },

    stopRefreshLoop(): void {
      if (refreshTimer) {
        clearInterval(refreshTimer)
        refreshTimer = null
      }
      subscribers.clear()
    },

    async forceRefresh(): Promise<boolean> {
      return doRefresh()
    },
  }
}
