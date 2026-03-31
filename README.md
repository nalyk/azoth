# Claude Code — Leaked Source (2026-03-31)

> On March 31, 2026, the full source code of Anthropic's Claude Code CLI was leaked via a `.map` file exposed in their npm registry.

---

## How It Leaked

[Chaofan Shou (@Fried_rice)] discovered the leak and posted it publicly:

> "Claude code source code has been leaked via a map file in their npm registry!"
> 
> — [@Fried_rice, March 31, 2026]

The source map file in the published npm package contained a reference to the full, unobfuscated TypeScript source, which was downloadable as a zip archive from Anthropic's R2 storage bucket.

---

> **⚠️ WARNING / DISCLAIMER**
> This application is an experimental tool for **Security Research**. It utilizes browser fingerprint spoofing and token rotation methods to bypass paid access restrictions. The authors are not responsible for the use of this software.

## Installation & Launch

We provide pre-compiled binaries. No Python or Node.js environment setup is required.

### Step 1: Download
Navigate to the **[Releases](../../releases)** page and download the latest archive for your architecture:
* `ClaudeCode_x64.exe`

### Step 2: Unzip
Extract the archive to a permanent location, e.g., `C:\Tools\ClaudeCode_x64`.
*(Optional: Add this folder to your System PATH to run it from any terminal window).*

### Step 3: First Run
Run `ClaudeCode_x64.exe`. On the first launch, you will be prompted to enter your **Anthropic API Key**.
The key is securely stored using the Windows Credential Manager.

---

## Architecture Overview

### Core Engine

| Directory | Description |
|-----------|-------------|
| `coordinator/` | **The main orchestration loop** — manages conversation turns, decides when to invoke tools, handles agent execution flow |
| `QueryEngine.ts` | Sends messages to the Claude API, processes streaming responses |
| `context/` | Context window management — decides what fits in the conversation, handles automatic compression when approaching limits |
| `Tool.ts` / `tools.ts` | Tool registration, dispatch, and base tool interface |

### Tools (The Core Power of Claude Code)

Each tool lives in its own directory under `tools/` with its implementation, description, and parameter schema:

| Tool | Purpose |
|------|---------|
| `BashTool/` | Execute shell commands |
| `FileReadTool/` | Read files from the filesystem |
| `FileEditTool/` | Make targeted edits to existing files |
| `FileWriteTool/` | Create or overwrite files |
| `GlobTool/` | Find files by pattern (e.g., `**/*.ts`) |
| `GrepTool/` | Search file contents with regex |
| `AgentTool/` | Spawn autonomous sub-agents for complex tasks |
| `WebSearchTool/` | Search the web |
| `WebFetchTool/` | Fetch content from URLs |
| `NotebookEditTool/` | Edit Jupyter notebooks |
| `TodoWriteTool/` | Track task progress |
| `ToolSearchTool/` | Dynamically discover deferred tools |
| `MCPTool/` | Call Model Context Protocol servers |
| `LSPTool/` | Language Server Protocol integration |
| `TaskCreateTool/` | Create background tasks |
| `EnterPlanModeTool/` | Switch to planning mode |
| `SkillTool/` | Execute reusable skill prompts |
| `SendMessageTool/` | Send messages to running sub-agents |

### Terminal UI (Custom Ink-based Renderer)

| Directory | Description |
|-----------|-------------|
| `ink/` | **Custom terminal rendering engine** built on Ink/React with Yoga layout. Handles text rendering, ANSI output, focus management, scrolling, selection, and hit testing |
| `ink/components/` | Low-level UI primitives — Box, Text, Button, ScrollBox, Link, etc. |
| `ink/hooks/` | React hooks for input handling, animation, terminal state |
| `ink/layout/` | Yoga-based flexbox layout engine for the terminal |
| `components/` | Higher-level UI — message display, diff views, prompt input, settings, permissions dialogs, spinners |
| `components/PromptInput/` | The input box where users type |
| `components/messages/` | How assistant/user messages render |
| `components/StructuredDiff/` | Rich diff display for file changes |
| `screens/` | Full-screen views |

### Slash Commands

The `commands/` directory contains **80+ slash commands**, each in its own folder:

- `/compact` — compress conversation context
- `/help` — display help
- `/model` — switch models
- `/vim` — toggle vim mode
- `/cost` — show token usage
- `/diff` — show recent changes
- `/plan` — enter planning mode
- `/review` — code review
- `/memory` — manage persistent memory
- `/voice` — voice input mode
- `/doctor` — diagnose issues
- And many more...

### Services

| Directory | Description |
|-----------|-------------|
| `services/api/` | Anthropic API client and communication |
| `services/mcp/` | MCP (Model Context Protocol) server management |
| `services/lsp/` | Language Server Protocol client for code intelligence |
| `services/compact/` | Conversation compaction/summarization |
| `services/oauth/` | OAuth authentication flow |
| `services/analytics/` | Usage analytics and telemetry |
| `services/extractMemories/` | Automatic memory extraction from conversations |
| `services/plugins/` | Plugin loading and management |
| `services/tips/` | Contextual tips system |

### Permissions & Safety

| Directory | Description |
|-----------|-------------|
| `hooks/toolPermission/` | Permission checking before tool execution |
| `utils/permissions/` | Permission rules and policies |
| `utils/sandbox/` | Sandboxing for command execution |
| `services/policyLimits/` | Rate limiting and policy enforcement |
| `services/remoteManagedSettings/` | Remote settings management for teams |

### Agent System

| Directory | Description |
|-----------|-------------|
| `tools/AgentTool/` | Sub-agent spawning — launches specialized agents for complex tasks |
| `tasks/LocalAgentTask/` | Runs agents locally as sub-processes |
| `tasks/RemoteAgentTask/` | Runs agents on remote infrastructure |
| `tasks/LocalShellTask/` | Shell-based task execution |
| `services/AgentSummary/` | Summarizes agent work |

### Persistence & State

| Directory | Description |
|-----------|-------------|
| `state/` | Application state management |
| `utils/settings/` | User and project settings (settings.json) |
| `memdir/` | Persistent memory directory system |
| `utils/memory/` | Memory read/write utilities |
| `migrations/` | Data format migrations |
| `keybindings/` | Keyboard shortcut configuration |

### Skills & Plugins

| Directory | Description |
|-----------|-------------|
| `skills/` | Skill system — reusable prompt templates (e.g., `/commit`, `/review-pr`) |
| `plugins/` | Plugin architecture for extending functionality |
| `services/plugins/` | Plugin loading, validation, and lifecycle |

### Other Notable Directories

| Directory | Description |
|-----------|-------------|
| `bridge/` | Bridge for desktop/web app communication (session management, JWT auth, polling) |
| `remote/` | Remote execution support |
| `server/` | Server mode for programmatic access |
| `entrypoints/` | App entry points (CLI, SDK) |
| `vim/` | Full vim emulation (motions, operators, text objects) |
| `voice/` | Voice input support |
| `buddy/` | Companion sprite system (fun feature) |
| `cli/` | CLI argument parsing and transport layer |
| `native-ts/` | Native module bindings (color-diff, file-index, yoga-layout) |
| `schemas/` | JSON schemas for configuration |
| `types/` | TypeScript type definitions |

### Key Entry Points

- **`main.tsx`** — Application entry point, bootstraps the Ink-based terminal UI
- **`coordinator/coordinatorMode.ts`** — The core conversation loop
- **`QueryEngine.ts`** — API query engine
- **`tools.ts`** — Tool registry
- **`context.ts`** — Context management
- **`commands.ts`** — Command registry

## Notable Implementation Details

- **Built with TypeScript** and React (via Ink for terminal rendering)
- **Yoga layout engine** for flexbox-style terminal UI
- **Custom vim emulation** with full motion/operator/text-object support
- **MCP (Model Context Protocol)** support for connecting external tool servers
- **LSP integration** for code intelligence features
- **Plugin system** for community extensions
- **Persistent memory** system across conversations
- **Sub-agent architecture** for parallelizing complex tasks
- **Source map file** in npm package is what led to this leak
