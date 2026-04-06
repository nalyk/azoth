/**
 * AZOTH CLI banner — displayed on daemon supervisor start and client commands.
 *
 * Design principles:
 * - Fits 80-column terminals
 * - Uses Unicode box-drawing and block elements (universal in 2026 terminals)
 * - Alchemical motif: the ouroboros circle, transmutation arrow
 * - Minimal height — respects terminal real estate
 * - Looks correct without color (but enhanced with ANSI when available)
 */

// ─── Color support detection ──────────────────────────────────────────────────

const NO_COLOR = 'NO_COLOR' in process.env
const FORCE_COLOR = 'FORCE_COLOR' in process.env

function supportsColor(): boolean {
  if (NO_COLOR) return false
  if (FORCE_COLOR) return true
  if (process.stdout.isTTY) return true
  return false
}

// ─── ANSI helpers (zero dependencies — no chalk needed) ───────────────────────

const esc = (code: string) => (s: string) =>
  supportsColor() ? `\x1b[${code}m${s}\x1b[0m` : s

const dim = esc('2')
const bold = esc('1')
const cyan = esc('36')
const gold = esc('33')       // yellow — closest to alchemical gold
const white = esc('97')
const gray = esc('90')
const red = esc('31')
const green = esc('32')
const magenta = esc('35')

// ─── The banner ───────────────────────────────────────────────────────────────

const LOGO = `
${gold('    ╔═══════════════════════════════════════════════════════╗')}
${gold('    ║')}                                                       ${gold('║')}
${gold('    ║')}     ${white('█████')}  ${white('██████')} ${white('██████')}  ${white('████████')} ${white('██')}  ${white('██')}            ${gold('║')}
${gold('    ║')}    ${white('██   ██')}      ${white('██')} ${white('██    ██')}    ${white('██')}    ${white('██')}  ${white('██')}            ${gold('║')}
${gold('    ║')}    ${white('███████')}  ${white('█████')}  ${white('██    ██')}    ${white('██')}    ${white('██████')}             ${gold('║')}
${gold('    ║')}    ${white('██   ██')} ${white('██')}      ${white('██    ██')}    ${white('██')}    ${white('██')}  ${white('██')}            ${gold('║')}
${gold('    ║')}    ${white('██   ██')} ${white('███████')}  ${white('██████')}     ${white('██')}    ${white('██')}  ${white('██')}            ${gold('║')}
${gold('    ║')}                                                       ${gold('║')}
${gold('    ║')}  ${dim('the universal solvent')}            ${dim('A·Z·O·T·H  v%VERSION%')}  ${gold('║')}
${gold('    ╚═══════════════════════════════════════════════════════╝')}
`

const LOGO_COMPACT = `${gold('◈')} ${bold(white('AZOTH'))} ${dim('v%VERSION%')} ${dim('— the universal solvent')}`

const SUPERVISOR_BANNER = `
${gold('    ╔═══════════════════════════════════════════════════════╗')}
${gold('    ║')}                                                       ${gold('║')}
${gold('    ║')}     ${white('█████')}  ${white('██████')} ${white('██████')}  ${white('████████')} ${white('██')}  ${white('██')}            ${gold('║')}
${gold('    ║')}    ${white('██   ██')}      ${white('██')} ${white('██    ██')}    ${white('██')}    ${white('██')}  ${white('██')}            ${gold('║')}
${gold('    ║')}    ${white('███████')}  ${white('█████')}  ${white('██    ██')}    ${white('██')}    ${white('██████')}             ${gold('║')}
${gold('    ║')}    ${white('██   ██')} ${white('██')}      ${white('██    ██')}    ${white('██')}    ${white('██')}  ${white('██')}            ${gold('║')}
${gold('    ║')}    ${white('██   ██')} ${white('███████')}  ${white('██████')}     ${white('██')}    ${white('██')}  ${white('██')}            ${gold('║')}
${gold('    ║')}                                                       ${gold('║')}
${gold('    ║')}  ${dim('daemon supervisor')}     ${dim('socket:')} ${cyan('%SOCKET%')}  ${gold('║')}
${gold('    ║')}  ${dim('pid')} ${white('%PID%')}                ${dim('workers:')} ${green('%WORKERS%')}${dim('/%MAX%')}         ${gold('║')}
${gold('    ╚═══════════════════════════════════════════════════════╝')}
`

// ─── Status line fragments ────────────────────────────────────────────────────

const STATUS_BULLET = gold('◈')
const STATUS_ARROW = gold('→')
const STATUS_CHECK = green('✓')
const STATUS_CROSS = red('✗')
const STATUS_PENDING = gold('◌')
const STATUS_ACTIVE = cyan('●')

// ─── Public API ───────────────────────────────────────────────────────────────

export type BannerOpts = {
  version?: string
  socketPath?: string
  pid?: number
  workers?: number
  maxWorkers?: number
  compact?: boolean
}

/**
 * Render the AZOTH startup banner.
 * Returns a string — caller writes to stdout/stderr.
 */
export function renderBanner(opts: BannerOpts = {}): string {
  const version = opts.version ?? 'dev'

  if (opts.compact) {
    return LOGO_COMPACT.replace('%VERSION%', version) + '\n'
  }

  return LOGO.replace('%VERSION%', version) + '\n'
}

/**
 * Render the supervisor startup banner with runtime info.
 */
export function renderSupervisorBanner(opts: BannerOpts = {}): string {
  const version = opts.version ?? 'dev'
  const socket = opts.socketPath ?? '~/.claude/daemon.sock'
  const pid = String(opts.pid ?? process.pid)
  const workers = String(opts.workers ?? 0)
  const max = String(opts.maxWorkers ?? 8)

  // Pad values to fit fixed-width layout
  const socketDisplay = socket.length > 24
    ? '~' + socket.slice(socket.length - 23)
    : socket.padEnd(24)
  const pidDisplay = pid.padEnd(8)
  const workersDisplay = workers
  const maxDisplay = max

  return SUPERVISOR_BANNER
    .replace('%SOCKET%', socketDisplay)
    .replace('%PID%', pidDisplay)
    .replace('%WORKERS%', workersDisplay)
    .replace('%MAX%', maxDisplay)
    .replace('%VERSION%', version) + '\n'
}

/**
 * Render the status header for `azoth daemon status`.
 */
export function renderStatusHeader(opts: {
  version: string
  uptime: string
  workers: number
  maxWorkers: number
  sessions: number
  pendingPermissions: number
}): string {
  const lines = [
    '',
    `  ${STATUS_BULLET} ${bold(white('AZOTH'))} ${dim('v' + opts.version)}`,
    '',
    `  ${dim('uptime')}   ${white(opts.uptime)}`,
    `  ${dim('workers')}  ${opts.workers > 0 ? green(String(opts.workers)) : gray('0')}${dim('/' + opts.maxWorkers)}`,
    `  ${dim('sessions')} ${opts.sessions > 0 ? cyan(String(opts.sessions)) : gray('0')}`,
    opts.pendingPermissions > 0
      ? `  ${dim('pending')}  ${gold(String(opts.pendingPermissions))} ${dim('permission requests')}`
      : '',
    '',
  ].filter(Boolean)

  return lines.join('\n') + '\n'
}

/**
 * Format a worker row for the status table.
 */
export function formatWorkerRow(w: {
  key: string
  pid: number
  status: string
  activeSessions: number
  capacity: number
  uptimeMs: number
}): string {
  const statusColors: Record<string, (s: string) => string> = {
    running: green,
    starting: gold,
    parked: red,
    restarting: gold,
    stopping: gray,
  }
  const colorFn = statusColors[w.status] ?? dim

  const uptime = formatDuration(w.uptimeMs)
  const sessions = `${w.activeSessions}/${w.capacity}`

  return [
    `  ${STATUS_ACTIVE} `,
    white(w.key.padEnd(44)),
    String(w.pid).padEnd(8),
    colorFn(w.status.padEnd(12)),
    sessions.padEnd(10),
    dim(uptime),
  ].join('')
}

/**
 * Worker table header.
 */
export function workerTableHeader(): string {
  return dim(
    `  ${'  '}${'DIRECTORY'.padEnd(44)}${'PID'.padEnd(8)}${'STATUS'.padEnd(12)}${'SESSIONS'.padEnd(10)}UPTIME`,
  )
}

/**
 * Format a permission request notification.
 */
export function formatPermissionRequest(req: {
  requestId: string
  toolName: string
  description: string
  workerKey: string
}): string {
  return [
    '',
    `  ${gold('╭──')} ${bold('Permission Request')} ${dim(req.requestId)}`,
    `  ${gold('│')}  ${dim('worker')}  ${white(req.workerKey)}`,
    `  ${gold('│')}  ${dim('tool')}    ${white(req.toolName)}`,
    `  ${gold('│')}  ${dim('action')}  ${req.description}`,
    `  ${gold('│')}`,
    `  ${gold('│')}  ${green('azoth daemon approve ' + req.requestId)}`,
    `  ${gold('│')}  ${red('azoth daemon deny ' + req.requestId)}`,
    `  ${gold('╰──')}`,
    '',
  ].join('\n')
}

/**
 * Log line prefix for daemon output.
 */
export function logPrefix(level: 'info' | 'warn' | 'error' | 'debug' = 'info'): string {
  const ts = new Date().toISOString().slice(11, 19)
  const levelColors: Record<string, (s: string) => string> = {
    info: dim,
    warn: gold,
    error: red,
    debug: gray,
  }
  const colorFn = levelColors[level] ?? dim
  return `${gray(ts)} ${colorFn(level.padEnd(5))} ${gold('◈')} `
}

// ─── Utility ──────────────────────────────────────────────────────────────────

function formatDuration(ms: number): string {
  const s = Math.floor(ms / 1000)
  const m = Math.floor(s / 60)
  const h = Math.floor(m / 60)
  const d = Math.floor(h / 24)

  if (d > 0) return `${d}d ${h % 24}h`
  if (h > 0) return `${h}h ${m % 60}m`
  if (m > 0) return `${m}m ${s % 60}s`
  return `${s}s`
}
