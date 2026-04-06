#!/bin/bash
# Generate stub modules for all src/* imports referenced by bridge/ and cli/
# Each stub exports empty functions/objects that satisfy TypeScript imports

STUBS_DIR="$(dirname "$0")"

# Generic stub content — exports common patterns as no-ops
stub() {
  local file="$STUBS_DIR/$1"
  mkdir -p "$(dirname "$file")"
  cat > "$file"
}

# ─── Bootstrap ─────────────────────────────────────────────────────────────────

stub "src/bootstrap/state.js" << 'EOF'
let _cwd = process.cwd()
export function setOriginalCwd(d) { _cwd = d }
export function setCwdState(d) { _cwd = d }
export function getOriginalCwd() { return _cwd }
export function getCwdState() { return _cwd }
EOF

# ─── Utils ─────────────────────────────────────────────────────────────────────

stub "src/utils/config.js" << 'EOF'
export function enableConfigs() {}
export function checkHasTrustDialogAccepted() { return true }
export function getGlobalConfig() { return {} }
export function getProjectConfig() { return {} }
export function setProjectConfig() {}
export function getFeatureValue_CACHED_MAY_BE_STALE() { return null }
export function getFeatureValue_CACHED_WITH_REFRESH() { return null }
export function checkGate_CACHED_OR_BLOCKING() { return true }
EOF

stub "src/utils/auth.js" << 'EOF'
export async function getClaudeAIOAuthTokens() {
  return { accessToken: process.env.CLAUDE_CODE_OAUTH_TOKEN || null, refreshToken: null }
}
export function clearOAuthTokenCache() {}
export async function checkAndRefreshOAuthTokenIfNeeded() { return true }
export function getOAuthBaseUrl() { return 'https://claude.ai' }
EOF

stub "src/utils/sinks.js" << 'EOF'
export function initSinks() {}
EOF

stub "src/utils/debug.js" << 'EOF'
export function logForDebugging(msg, opts) {}
EOF

stub "src/utils/errors.js" << 'EOF'
export function isENOENT(e) { return e?.code === 'ENOENT' }
export function isEACCES(e) { return e?.code === 'EACCES' }
EOF

stub "src/utils/git.js" << 'EOF'
export function getBranch() { return 'main' }
export function getRemoteUrl() { return null }
export function findGitRoot(dir) { return dir }
EOF

stub "src/utils/hooks.js" << 'EOF'
export function hasWorktreeCreateHook() { return false }
export function executePermissionRequestHooks() { return { behavior: 'allow' } }
EOF

stub "src/utils/slowOperations.js" << 'EOF'
export function jsonParse(s) { return JSON.parse(s) }
export function jsonStringify(o) { return JSON.stringify(o) }
EOF

stub "src/utils/uuid.js" << 'EOF'
import { randomUUID } from 'crypto'
export function generateUUID() { return randomUUID() }
EOF

stub "src/utils/sessionStoragePortable.js" << 'EOF'
import { homedir } from 'os'
import { join } from 'path'
export function getProjectsDir() { return join(homedir(), '.claude', 'projects') }
export function sanitizePath(p) { return p.replace(/[^a-zA-Z0-9_-]/g, '_') }
EOF

stub "src/utils/getWorktreePathsPortable.js" << 'EOF'
export function getWorktreePathsPortable() { return [] }
EOF

stub "src/utils/lazySchema.js" << 'EOF'
export function lazySchema(fn) { return fn }
EOF

# Generate empty stubs for all remaining src/* modules
for mod in \
  "src/utils/abortController.js" \
  "src/utils/array.js" \
  "src/utils/autoUpdater.js" \
  "src/utils/awsAuthStatusManager.js" \
  "src/utils/betas.js" \
  "src/utils/cleanupRegistry.js" \
  "src/utils/combinedAbortSignal.js" \
  "src/utils/commandLifecycle.js" \
  "src/utils/commitAttribution.js" \
  "src/utils/completionCache.js" \
  "src/utils/conversationRecovery.js" \
  "src/utils/cwd.js" \
  "src/utils/diagLogs.js" \
  "src/utils/doctorDiagnostic.js" \
  "src/utils/effort.js" \
  "src/utils/fastMode.js" \
  "src/utils/fileHistory.js" \
  "src/utils/fileStateCache.js" \
  "src/utils/forkedAgent.js" \
  "src/utils/generators.js" \
  "src/utils/gracefulShutdown.js" \
  "src/utils/headlessProfiler.js" \
  "src/utils/idleTimeout.js" \
  "src/utils/json.js" \
  "src/utils/localInstaller.js" \
  "src/utils/log.js" \
  "src/utils/messageQueueManager.js" \
  "src/utils/messages.js" \
  "src/utils/path.js" \
  "src/utils/process.js" \
  "src/utils/queryContext.js" \
  "src/utils/queryHelpers.js" \
  "src/utils/queryProfiler.js" \
  "src/utils/semver.js" \
  "src/utils/sessionRestore.js" \
  "src/utils/sessionStart.js" \
  "src/utils/sessionState.js" \
  "src/utils/sessionStorage.js" \
  "src/utils/sessionTitle.js" \
  "src/utils/sessionUrl.js" \
  "src/utils/sideQuestion.js" \
  "src/utils/stream.js" \
  "src/utils/streamJsonStdoutGuard.js" \
  "src/utils/streamlinedTransform.js" \
  "src/utils/thinking.js" \
  "src/utils/toolPool.js" \
  "src/utils/workloadContext.js" \
  "src/utils/filePersistence/filePersistence.js" \
  "src/utils/hooks/AsyncHookRegistry.js" \
  "src/utils/hooks/hookEvents.js" \
  "src/utils/messages/mappers.js" \
  "src/utils/model/model.js" \
  "src/utils/model/modelOptions.js" \
  "src/utils/model/modelStrings.js" \
  "src/utils/model/providers.js" \
  "src/utils/nativeInstaller/index.js" \
  "src/utils/nativeInstaller/packageManagers.js" \
  "src/utils/permissions/PermissionPromptToolResultSchema.js" \
  "src/utils/permissions/PermissionResult.js" \
  "src/utils/permissions/permissionSetup.js" \
  "src/utils/permissions/permissions.js" \
  "src/utils/plugins/pluginIdentifier.js" \
  "src/utils/sandbox/sandbox-adapter.js" \
  "src/utils/settings/applySettingsChange.js" \
  "src/utils/settings/changeDetector.js" \
  "src/utils/settings/settings.js" \
  "src/services/analytics/growthbook.js" \
  "src/services/analytics/index.js" \
  "src/services/api/grove.js" \
  "src/services/api/logging.js" \
  "src/services/claudeAiLimits.js" \
  "src/services/mcp/auth.js" \
  "src/services/mcp/channelAllowlist.js" \
  "src/services/mcp/channelNotification.js" \
  "src/services/mcp/client.js" \
  "src/services/mcp/config.js" \
  "src/services/mcp/elicitationHandler.js" \
  "src/services/mcp/mcpStringUtils.js" \
  "src/services/mcp/types.js" \
  "src/services/mcp/utils.js" \
  "src/services/mcp/vscodeSdkMcp.js" \
  "src/services/oauth/index.js" \
  "src/services/policyLimits/index.js" \
  "src/services/remoteManagedSettings/index.js" \
  "src/services/settingsSync/index.js" \
  "src/services/PromptSuggestion/promptSuggestion.js" \
  "src/state/AppStateStore.js" \
  "src/state/onChangeAppState.js" \
  "src/entrypoints/agentSdkTypes.js" \
  "src/entrypoints/sdk/controlSchemas.js" \
  "src/entrypoints/sdk/controlTypes.js" \
  "src/types/hooks.js" \
  "src/types/ids.js" \
  "src/types/message.js" \
  "src/types/permissions.js" \
  "src/types/textInputTypes.js" \
  "src/hooks/useCanUseTool.js" \
  "src/constants/outputStyles.js" \
  "src/constants/product.js" \
  "src/constants/xml.js" \
  "src/commands/context/context-noninteractive.js" \
  "src/commands.js" \
  "src/QueryEngine.js" \
  "src/Tool.js" \
  "src/tools.js" \
  "src/tools/AgentTool/loadAgentsDir.js" \
  "src/tools/SyntheticOutputTool/SyntheticOutputTool.js" \
  "src/bridge/bridgeStatusUtil.js" \
  "src/bridge/inboundAttachments.js" \
  "src/bridge/inboundMessages.js" \
  "src/bridge/replBridge.js" \
  "src/cli/handlers/auth.js" \
  "src/cli/remoteIO.js" \
  "src/cli/structuredIO.js"
do
  if [ ! -f "$STUBS_DIR/$mod" ]; then
    stub "$mod" << 'GENERICEOF'
// Auto-generated stub
export default {}
GENERICEOF
  fi
done

echo "Stubs generated in $STUBS_DIR/src/"
find "$STUBS_DIR/src" -name "*.js" | wc -l
echo "stub files created"
