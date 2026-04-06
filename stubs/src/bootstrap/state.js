let _cwd = process.cwd()
export function setOriginalCwd(d) { _cwd = d }
export function setCwdState(d) { _cwd = d }
export function getOriginalCwd() { return _cwd }
export function getCwdState() { return _cwd }
