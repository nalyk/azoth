#!/bin/bash
# Create stub packages for internal Anthropic modules not on npm.
# Run after every `bun install` since it clears node_modules.

NM="$(dirname "$0")/../node_modules"

stub_pkg() {
  local pkg="$1"
  shift
  mkdir -p "$NM/$pkg"
  echo "{\"name\":\"$pkg\",\"version\":\"0.0.0-stub\",\"main\":\"index.js\",\"type\":\"module\"}" > "$NM/$pkg/package.json"

  # Write exports
  {
    for exp in "$@"; do
      echo "export function $exp() { return null }"
    done
    echo "export default {}"
  } > "$NM/$pkg/index.js"
}

# @alcalzone/ansi-tokenize
stub_pkg "@alcalzone/ansi-tokenize" reduceAnsiCodes tokenize ansiCodesToString undoAnsiCodes diffAnsiCodes styledCharsFromTokens

# @ant packages
stub_pkg "@ant/claude-for-chrome-mcp"
echo "export const BROWSER_TOOLS = []; export default { BROWSER_TOOLS: [] };" > "$NM/@ant/claude-for-chrome-mcp/index.js"

stub_pkg "@ant/computer-use-input"
stub_pkg "@ant/computer-use-mcp"
mkdir -p "$NM/@ant/computer-use-mcp"
echo "export default {};" > "$NM/@ant/computer-use-mcp/sentinelApps.js"
echo "export default {};" > "$NM/@ant/computer-use-mcp/types.js"
stub_pkg "@ant/computer-use-swift"

# @anthropic-ai internal
stub_pkg "@anthropic-ai/claude-agent-sdk"
stub_pkg "@anthropic-ai/mcpb"

# @anthropic-ai/sandbox-runtime
mkdir -p "$NM/@anthropic-ai/sandbox-runtime"
echo "{\"name\":\"@anthropic-ai/sandbox-runtime\",\"version\":\"0.0.0-stub\",\"main\":\"index.js\",\"type\":\"module\"}" > "$NM/@anthropic-ai/sandbox-runtime/package.json"
cat > "$NM/@anthropic-ai/sandbox-runtime/index.js" << 'SANDBOX'
import { z } from 'zod'
export const SandboxRuntimeConfigSchema = z.object({}).passthrough().optional()
export class SandboxViolationStore { constructor(){} add(){} get(){ return [] } clear(){} size(){ return 0 } }
export class SandboxManager { constructor(){} async start(){} async stop(){} isEnabled(){ return false } checkAccess(){ return true } getViolations(){ return [] } }
export function createSandbox() { return new SandboxManager() }
export default {}
SANDBOX

# OpenTelemetry
export const ATTR_SERVICE_NAME = "service.name"; export const ATTR_SERVICE_VERSION = "service.version"; export default {}
EOF
export function resourceFromAttributes() { return {} }; export class Resource { constructor(){} }; export default {}
EOF
export class LoggerProvider { constructor(){} getLogger(){ return { emit(){} } } shutdown(){ return Promise.resolve() } }
export class BatchLogRecordProcessor { constructor(){} shutdown(){ return Promise.resolve() } }
export class SimpleLogRecordProcessor { constructor(){} }; export class ConsoleLogRecordExporter {}; export default {}
EOF

# VSCode LSP
for vsc in vscode-jsonrpc vscode-languageserver-protocol vscode-languageserver-types; do
  mkdir -p "$NM/$vsc"
  echo "export default {};" > "$NM/$vsc/index.js"
  echo "{\"name\":\"$vsc\",\"version\":\"0.0.0-stub\",\"main\":\"index.js\",\"type\":\"module\"}" > "$NM/$vsc/package.json"
done
mkdir -p "$NM/vscode-jsonrpc/node"
echo "export default {};" > "$NM/vscode-jsonrpc/node.js"

# Other stubs
for pkg in "color-diff-napi" "asciichart" "bidi-js" "usehooks-ts" "google-auth-library" "@aws-sdk/client-bedrock-runtime"; do
  stub_pkg "$pkg"
done
echo "export class GoogleAuth { constructor(){} getClient(){ return {} } };" > "$NM/google-auth-library/index.js"
echo "export class BedrockRuntimeClient { constructor(){} send(){} }; export class InvokeModelWithResponseStreamCommand {}; export class InvokeModelCommand {};" > "$NM/@aws-sdk/client-bedrock-runtime/index.js"
stub_pkg "usehooks-ts" useEventCallback

# react/compiler-runtime
mkdir -p "$NM/react/compiler-runtime"
echo '{"name":"react-compiler-runtime","version":"0.0.0","main":"index.js","type":"module"}' > "$NM/react/compiler-runtime/package.json"
echo 'export function c(size) { return new Array(size).fill(Symbol.for("react.memo_cache_sentinel")); }; export default { c };' > "$NM/react/compiler-runtime/index.js"

# Patch react exports for compiler-runtime subpath
if command -v python3 &>/dev/null && [ -f "$NM/react/package.json" ]; then
  python3 -c "
import json
with open('$NM/react/package.json') as f: pkg = json.load(f)
if 'exports' in pkg:
    pkg['exports']['./compiler-runtime'] = {'default': './compiler-runtime/index.js'}
    with open('$NM/react/package.json','w') as f: json.dump(pkg, f, indent=2)
" 2>/dev/null
fi

echo "Internal stubs installed."
