/**
 * LRU conversation cache for the daemon supervisor.
 *
 * Caches recent session messages parsed from the NDJSON relay stream
 * that workers forward. Serves instant conversation history to CLI
 * clients via `claude daemon logs --session <id>` without a network
 * round trip to the Anthropic sessions API.
 *
 * Source of truth remains the sessions API — this is a read-only,
 * best-effort, local-only optimization.
 */

// ─── Types ─────────────────────────────────────────────────────��──────────────

export type CachedMessage = {
  role: 'user' | 'assistant' | 'system'
  content: string
  timestamp: number
  sessionId: string
}

export type ConversationCache = {
  /** Add a message to a session's cache. */
  push(sessionId: string, message: CachedMessage): void

  /** Get cached messages for a session (oldest-first). */
  get(sessionId: string): CachedMessage[]

  /** Get all cached session IDs. */
  sessions(): string[]

  /** Remove a session's cache. */
  evict(sessionId: string): void

  /** Clear all caches. */
  clear(): void

  /** Current number of cached sessions. */
  size: number
}

// ─── Implementation ────────────────────────────────────────────────��──────────

export function createConversationCache(opts: {
  /** Max messages per session. */
  maxMessagesPerSession: number
  /** Max sessions to cache. */
  maxSessions: number
}): ConversationCache {
  const { maxMessagesPerSession, maxSessions } = opts

  // LRU via Map insertion order (most-recently-accessed at the end)
  const cache = new Map<string, CachedMessage[]>()

  function touch(sessionId: string): CachedMessage[] {
    const messages = cache.get(sessionId)
    if (messages) {
      // Move to end (most recent)
      cache.delete(sessionId)
      cache.set(sessionId, messages)
      return messages
    }
    return []
  }

  function evictOldest(): void {
    if (cache.size <= maxSessions) return
    // Map keys iterate in insertion order — first key is the LRU
    const oldest = cache.keys().next().value
    if (oldest !== undefined) {
      cache.delete(oldest)
    }
  }

  return {
    push(sessionId: string, message: CachedMessage): void {
      let messages = cache.get(sessionId)
      if (!messages) {
        messages = []
        cache.set(sessionId, messages)
        evictOldest()
      } else {
        touch(sessionId)
      }

      messages.push(message)

      // Trim to max depth
      if (messages.length > maxMessagesPerSession) {
        messages.splice(0, messages.length - maxMessagesPerSession)
      }
    },

    get(sessionId: string): CachedMessage[] {
      return touch(sessionId).slice() // Return a copy
    },

    sessions(): string[] {
      return Array.from(cache.keys())
    },

    evict(sessionId: string): void {
      cache.delete(sessionId)
    },

    clear(): void {
      cache.clear()
    },

    get size(): number {
      return cache.size
    },
  }
}

// ─── NDJSON log parser for conversation extraction ──────────────────��─────────

/**
 * Parse a worker log line and extract a conversation message if present.
 * Worker logs are NDJSON from the child session's stdout relay.
 */
export function parseLogForMessage(
  sessionId: string,
  logLine: string,
): CachedMessage | null {
  try {
    const parsed = JSON.parse(logLine)

    // User messages (from --replay-user-messages)
    if (parsed.type === 'user' && typeof parsed.content === 'string') {
      return {
        role: 'user',
        content: parsed.content,
        timestamp: Date.now(),
        sessionId,
      }
    }

    // Assistant text output
    if (parsed.type === 'text' && typeof parsed.text === 'string') {
      return {
        role: 'assistant',
        content: parsed.text,
        timestamp: Date.now(),
        sessionId,
      }
    }

    // Stream event with assistant content
    if (
      parsed.type === 'stream_event' &&
      parsed.event?.type === 'content_block_delta' &&
      parsed.event?.delta?.type === 'text_delta'
    ) {
      return {
        role: 'assistant',
        content: parsed.event.delta.text,
        timestamp: Date.now(),
        sessionId,
      }
    }

    return null
  } catch {
    return null
  }
}
