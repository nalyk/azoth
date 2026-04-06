import { homedir } from 'os'
import { join } from 'path'
export function getProjectsDir() { return join(homedir(), '.claude', 'projects') }
export function sanitizePath(p) { return p.replace(/[^a-zA-Z0-9_-]/g, '_') }
