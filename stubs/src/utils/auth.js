export async function getClaudeAIOAuthTokens() {
  return { accessToken: process.env.CLAUDE_CODE_OAUTH_TOKEN || null, refreshToken: null }
}
export function clearOAuthTokenCache() {}
export async function checkAndRefreshOAuthTokenIfNeeded() { return true }
export function getOAuthBaseUrl() { return 'https://claude.ai' }
