/**
 * Stub for bun:bundle compile-time feature flags.
 * Runtime polyfill — all flags default to false.
 */

const ENABLED_FEATURES = new Set<string>([
  // Uncomment to enable:
  // 'KAIROS',
  // 'PROACTIVE',
  // 'BRIDGE_MODE',
  // 'VOICE_MODE',
  // 'COORDINATOR_MODE',
  // 'BASH_CLASSIFIER',
  // 'TRANSCRIPT_CLASSIFIER',
  // 'BUDDY',
  // 'WEB_BROWSER_TOOL',
  // 'CHICAGO_MCP',
  // 'AGENT_TRIGGERS',
  // 'ULTRAPLAN',
  // 'MONITOR_TOOL',
  // 'TEAMMEM',
  // 'EXTRACT_MEMORIES',
  // 'MCP_SKILLS',
])

export function feature(name: string): boolean {
  return ENABLED_FEATURES.has(name)
}

export const MACRO = {
  VERSION: '0.1.0',
  BUILD_DATE: new Date().toISOString(),
}
