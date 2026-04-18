//! `bash` — run a shell command inside the repo root. The process inherits
//! only a sanitized environment. Output is capped to prevent context
//! blowout. Respects the execution context's cancellation token.
//!
//! ## v2.1 sandbox wiring (Gap 2 closure)
//!
//! When `AZOTH_SANDBOX` is `tier_a` or `tier_b`, bash no longer runs
//! against the host FS. Instead:
//!
//! - `tier_a` — bash runs inside the unprivileged user-ns + net-ns +
//!   Landlock jail. Landlock allow_read covers `/bin`, `/lib`,
//!   `/lib64`, `/usr`, `/etc`, `/proc`, `/sys`, `/dev`, and the
//!   repo root. allow_write is `/tmp` only. Writes to the real
//!   repo or `/etc/passwd` etc. return EACCES.
//!
//! - `tier_b` — Tier A plus a `fuse-overlayfs` mount rooted at the
//!   repo: bash's cwd is the merged view, so writes land in an
//!   upper layer the dispatcher can collect via
//!   `OverlayWorkspace::changed_files()` at turn commit. Without
//!   fuse-overlayfs on PATH, the tool degrades to Tier A with a
//!   warning (same pattern as the rest of the runtime's
//!   best-effort degradation).
//!
//! Default (`off` or unset) matches v2.0.0 behaviour — bash runs
//! directly via `tokio::process::Command`.

use crate::execution::{ExecutionContext, Tool, ToolError};
use crate::sandbox::SandboxPolicy;
use crate::schemas::EffectClass;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::process::Command;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Platform-portable handle that holds the Tier B overlay on
/// Linux and is a unit elsewhere. Lets `execute()` declare
/// `let mut overlay_handle: OverlayHandle = Default::default();`
/// without scattering `#[cfg]` through the happy path (codex
/// re-review P1 on PR #14).
#[cfg(target_os = "linux")]
type OverlayHandle = Option<crate::sandbox::OverlayWorkspace>;
#[cfg(not(target_os = "linux"))]
type OverlayHandle = ();

pub struct BashTool;

#[derive(Debug, Deserialize)]
pub struct BashInput {
    pub command: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct BashOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
    /// Relative paths of files bash wrote during this invocation.
    /// Under `SandboxPolicy::TierB`, these are the files the tool
    /// staged from the fuse-overlayfs upper layer back to the real
    /// repo root before the overlay was unmounted. Empty for
    /// `SandboxPolicy::Off` and `SandboxPolicy::TierA` (the real
    /// repo is written directly; no overlay to reconcile) and for
    /// failed invocations (non-zero exit, timeout, cancellation)
    /// where Tier B's discard-on-failure semantics mean nothing
    /// should leak out of the sandbox.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub staged_files: Vec<String>,
    /// Relative paths of files bash DELETED during this invocation.
    /// Under `SandboxPolicy::TierB`, fuse-overlayfs records
    /// deletions as whiteouts (character-device entries or
    /// `.wh.<name>` markers) in the upper layer; this field
    /// surfaces them as first-class "removed" signals after
    /// `stage_overlay_back` has propagated the delete to the
    /// real repo root. Empty for Off/TierA and for failed
    /// invocations. Codex round-4 P1 on PR #14 — without
    /// whiteout handling, `rm` under Tier B silently reverted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_files: Vec<String>,
}

#[async_trait]
impl Tool for BashTool {
    type Input = BashInput;
    type Output = BashOutput;

    fn name(&self) -> &'static str {
        "bash"
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::ApplyLocal
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "timeout_ms": { "type": "integer", "minimum": 1000, "maximum": 600000 }
            },
            "required": ["command"]
        })
    }

    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ExecutionContext,
    ) -> Result<Self::Output, ToolError> {
        let timeout = std::time::Duration::from_millis(
            input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(600_000),
        );

        // v2.1 Gap 2 closure — route through the sandbox when asked.
        let policy = SandboxPolicy::from_env();
        // Build a lazy-mounted overlay for Tier B *only* when the
        // policy asks for it. The handle is held on the stack so
        // `Drop` unmounts when execute() returns, whether the bash
        // run committed or failed. `OverlayHandle` is a
        // cross-platform type alias (`Option<OverlayWorkspace>` on
        // Linux, `()` elsewhere) so the outer code stays portable
        // per codex re-review P1 on PR #14.
        #[allow(unused_mut)]
        let mut overlay_handle: OverlayHandle = Default::default();
        // v2.1 codex re-review round 3 P1: the up-front user-ns
        // probe + build-error handling catch the common
        // degradation cases, but the REAL jail sequence
        // (CLONE_NEWNET, Landlock, seccomp) runs in `pre_exec`
        // inside the actual fork. On a container that allows
        // unprivileged user-ns but denies net-ns or Landlock,
        // build_bash_command returns Ok, and spawn() then fails
        // with EPERM. Catch that and retry unsandboxed once.
        // Only retry when the original policy was sandboxed —
        // a genuine "bash not installed" must still propagate.
        let spawn_attempt =
            build_bash_command(&input.command, &ctx.repo_root, policy, &mut overlay_handle)?
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn();
        let mut child = match spawn_attempt {
            Ok(c) => c,
            Err(e) if !policy.is_off() => {
                tracing::warn!(
                    error = %e,
                    policy = ?policy,
                    "sandboxed spawn failed (likely pre_exec EPERM on net-ns / landlock \
                     in a container); degrading to unsandboxed for this invocation"
                );
                // Drop any mounted overlay before retrying — the
                // retry writes to the real repo, so staging from
                // an abandoned upper layer would be wrong.
                overlay_handle = Default::default();
                build_bash_command(
                    &input.command,
                    &ctx.repo_root,
                    SandboxPolicy::Off,
                    &mut overlay_handle,
                )?
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| ToolError::Failed(format!("spawn (unsandboxed retry): {e}")))?
            }
            Err(e) => return Err(ToolError::Failed(format!("spawn: {e}"))),
        };

        let wait_fut = async {
            let status = child.wait().await?;
            let mut stdout_buf = Vec::new();
            let mut stderr_buf = Vec::new();
            if let Some(mut out) = child.stdout.take() {
                tokio::io::AsyncReadExt::read_to_end(&mut out, &mut stdout_buf).await?;
            }
            if let Some(mut err) = child.stderr.take() {
                tokio::io::AsyncReadExt::read_to_end(&mut err, &mut stderr_buf).await?;
            }
            Ok::<_, std::io::Error>((status, stdout_buf, stderr_buf))
        };

        let result = tokio::select! {
            biased;
            _ = ctx.cancellation.wait_cancelled() => {
                let _ = child.kill().await;
                return Err(ToolError::Cancelled);
            }
            r = tokio::time::timeout(timeout, wait_fut) => r,
        };

        match result {
            Ok(Ok((status, stdout_buf, stderr_buf))) => {
                let (stdout, stderr, truncated) = cap_output(&stdout_buf, &stderr_buf);
                // Tier B closure (PR #14 codex P1 fix): on
                // successful exit, copy the overlay's upper-layer
                // writes back into the real repo BEFORE the handle
                // Drops (which unmounts fuse-overlayfs and deletes
                // the tmpdir). Failed runs intentionally drop the
                // overlay untouched — that's the isolation
                // contract: bad turns leave the repo pristine.
                let stage = if status.success() {
                    stage_overlay_back(&overlay_handle, &ctx.repo_root)?
                } else {
                    StageResult::default()
                };
                Ok(BashOutput {
                    exit_code: status.code(),
                    stdout,
                    stderr,
                    truncated,
                    timed_out: false,
                    staged_files: stage.staged,
                    removed_files: stage.removed,
                })
            }
            Ok(Err(e)) => Err(ToolError::Failed(format!("wait: {e}"))),
            Err(_) => {
                let _ = child.kill().await;
                Ok(BashOutput {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: format!("timed out after {}ms", timeout.as_millis()),
                    truncated: false,
                    timed_out: true,
                    staged_files: Vec::new(),
                    removed_files: Vec::new(),
                })
            }
        }
    }
}

/// Two-list result surfaced by `stage_overlay_back`: files
/// written (`staged`) and files deleted (`removed`) by the bash
/// child. Keeping them separate lets the caller report semantic
/// changes explicitly rather than conflating "I created X" with
/// "I deleted X" — both are mutations, but they need different
/// UI and reasoning.
#[derive(Debug, Default)]
struct StageResult {
    staged: Vec<String>,
    removed: Vec<String>,
}

/// Reconcile the overlay's upper layer with `repo_root`.
/// - Regular files → copy into `repo_root`.
/// - fuse-overlayfs whiteouts → delete the corresponding path
///   in `repo_root`.
///
/// Called only when bash exits with status 0 under Tier B —
/// failed runs do not stage.
///
/// ## Whiteout conventions (codex round-4 P1 on PR #14)
///
/// When bash runs `rm foo.rs` under fuse-overlayfs, the
/// deletion lands in the upper layer as one of two markers:
///
/// 1. **Character device with major:minor = 0:0** — the
///    standard overlayfs whiteout (mknod c 0 0). Detected via
///    `FileTypeExt::is_char_device()`.
/// 2. **`.wh.<name>` regular file** — an alternate naming
///    convention used by fuse-overlayfs in some modes
///    (particularly without CAP_MKNOD). Detected by filename
///    prefix.
///
/// Either way the semantic is "delete the corresponding lower
/// path". Without this handling, the earlier implementation
/// blindly `std::fs::copy`-d the whiteout: either erroring on
/// the character-device case or copying a literal `.wh.foo.rs`
/// artifact into the real repo. Codex P1 verbatim: *"That
/// leaves the repository state inconsistent with the command's
/// exit status and can break turn replay/forensics."*
///
/// Opaque-directory whiteouts (`.wh..wh..opq` markers, or the
/// `trusted.overlay.opaque=y` xattr) are NOT handled yet —
/// reading `trusted.*` xattrs requires `CAP_SYS_ADMIN`, which
/// the unprivileged user-ns child does not have. Whole-subtree
/// deletions under Tier B remain a v2.5 scope item; for v2.1,
/// bash + rm + single-file is the supported semantic.
#[cfg(target_os = "linux")]
fn stage_overlay_back(
    overlay: &OverlayHandle,
    repo_root: &std::path::Path,
) -> Result<StageResult, ToolError> {
    use std::os::unix::fs::FileTypeExt;

    let mut result = StageResult::default();
    let Some(ws) = overlay.as_ref() else {
        return Ok(result);
    };
    let rels = ws
        .changed_files()
        .map_err(|e| ToolError::Failed(format!("overlay changed_files: {e}")))?;
    // Belt-and-braces for the symlink-directory traversal defence
    // (codex round-6 P1): even after fixing the walker in
    // `collect_files` to stop following symlinked dirs, refuse to
    // stage any entry whose resolved source path would escape
    // `ws.upper`. `canonicalize()` resolves every symlink in the
    // path; if the fully-resolved src leaves the upper layer, we
    // skip with a warning rather than copying attacker-chosen
    // host content into the real repo.
    let upper_canon = ws.upper.canonicalize().map_err(|e| {
        ToolError::Failed(format!("canonicalize upper {}: {e}", ws.upper.display()))
    })?;
    for rel in &rels {
        let src = ws.upper.join(rel);
        let dst = repo_root.join(rel);
        let meta = std::fs::symlink_metadata(&src)
            .map_err(|e| ToolError::Failed(format!("stat upper {}: {e}", src.display())))?;

        // Symlinks: canonicalize would follow them, which is
        // exactly what we DON'T want to check here — we record
        // symlinks as-is (see the branch below). Regular
        // non-symlink entries go through the boundary check.
        if !meta.file_type().is_symlink() {
            match src.canonicalize() {
                Ok(canon) if canon.starts_with(&upper_canon) => {}
                Ok(canon) => {
                    tracing::warn!(
                        rel = %rel.display(),
                        resolved = %canon.display(),
                        upper = %upper_canon.display(),
                        "stage-back refused: resolved source escapes overlay upper layer \
                         (possible symlink-traversal attack)"
                    );
                    continue;
                }
                Err(e) => {
                    // Canonicalization can fail for whiteout
                    // char-devices or broken-link targets —
                    // both of which are handled by the branches
                    // below. Fall through; don't treat this as
                    // fatal.
                    tracing::debug!(
                        rel = %rel.display(),
                        error = %e,
                        "canonicalize failed; deferring to type-specific branches"
                    );
                }
            }
        }

        // Whiteout convention 1: char device (standard overlayfs).
        if meta.file_type().is_char_device() {
            remove_at(&dst)?;
            result.removed.push(rel.display().to_string());
            continue;
        }

        // Whiteout convention 2: `.wh.<name>` prefix.
        if let Some(target_name) = rel
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix(".wh."))
        {
            let parent_rel = rel.parent().unwrap_or_else(|| std::path::Path::new(""));
            let target_rel = parent_rel.join(target_name);
            let target_dst = repo_root.join(&target_rel);
            remove_at(&target_dst)?;
            result.removed.push(target_rel.display().to_string());
            continue;
        }

        // Symlink — recreate via `std::os::unix::fs::symlink`.
        // Codex round-5 P1: the previous path called
        // `std::fs::copy` which dereferences symlinks, so a
        // successful `ln -s target link` in bash either
        // materialised the target's bytes as a regular file (wrong
        // semantic) or failed entirely if the target didn't
        // exist (turning bash success into a tool failure).
        if meta.file_type().is_symlink() {
            let link_target = std::fs::read_link(&src)
                .map_err(|e| ToolError::Failed(format!("readlink {}: {e}", src.display())))?;
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| ToolError::Failed(format!("stage mkdir_p: {e}")))?;
            }
            // Remove any prior entry at dst — either lower-layer
            // file or stale symlink — so `symlink(2)` doesn't
            // EEXIST.
            let _ = std::fs::remove_file(&dst);
            std::os::unix::fs::symlink(&link_target, &dst).map_err(|e| {
                ToolError::Failed(format!(
                    "stage symlink {} → {}: {e}",
                    link_target.display(),
                    dst.display()
                ))
            })?;
            result.staged.push(rel.display().to_string());
            continue;
        }

        // Regular file — copy.
        //
        // Codex round-7 P1: since round-6's walker fix now
        // records ALL file types (FIFOs, sockets, block
        // devices — not only regular files and char-device
        // whiteouts), we MUST gate the copy branch on
        // `is_file()` explicitly. `mkfifo pipe` in Tier B
        // would otherwise reach `std::fs::copy` which blocks
        // indefinitely waiting for a writer on the FIFO —
        // bash reports success, tool hangs. Sockets / block
        // devices would error or produce nonsense. Skip them
        // with a tracing::warn and move on; repos don't
        // carry IPC primitives.
        if !meta.file_type().is_file() {
            tracing::warn!(
                rel = %rel.display(),
                file_type = ?meta.file_type(),
                "stage-back: skipping non-regular upper-layer entry (fifo/socket/block-device)"
            );
            continue;
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ToolError::Failed(format!("stage mkdir_p: {e}")))?;
        }
        std::fs::copy(&src, &dst).map_err(|e| {
            ToolError::Failed(format!("stage {} → {}: {e}", src.display(), dst.display()))
        })?;
        result.staged.push(rel.display().to_string());
    }
    // Deterministic order so forensic replay is byte-stable.
    result.staged.sort();
    result.removed.sort();
    Ok(result)
}

/// Best-effort removal: try file delete first, fall back to
/// directory removal. Non-existent target is not an error (the
/// whiteout refers to a lower-layer path that may never have
/// existed in the real repo — that's still a valid "delete"
/// from the tool's perspective).
#[cfg(target_os = "linux")]
fn remove_at(path: &std::path::Path) -> Result<(), ToolError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        // `ErrorKind::IsADirectory` only stable in recent rustc
        // and clippy's MSRV check rejects it at current workspace
        // MSRV; key on the raw errno (EISDIR) which is stable
        // across every supported toolchain.
        Err(e) if e.raw_os_error() == Some(nix::libc::EISDIR) => std::fs::remove_dir_all(path)
            .map_err(|e| ToolError::Failed(format!("rmdir {}: {e}", path.display()))),
        Err(e) => Err(ToolError::Failed(format!("rm {}: {e}", path.display()))),
    }
}

/// Non-Linux stub so the caller's type and control flow stay
/// portable. No overlay exists on macOS/Windows/etc; nothing to
/// stage.
#[cfg(not(target_os = "linux"))]
fn stage_overlay_back(
    _overlay: &OverlayHandle,
    _repo_root: &std::path::Path,
) -> Result<StageResult, ToolError> {
    Ok(StageResult::default())
}

fn cap_output(stdout: &[u8], stderr: &[u8]) -> (String, String, bool) {
    let half = MAX_OUTPUT_BYTES / 2;
    let mut truncated = false;

    let out = if stdout.len() > half {
        truncated = true;
        let s = String::from_utf8_lossy(&stdout[..half]);
        format!("{s}\n... truncated ({} bytes total)", stdout.len())
    } else {
        String::from_utf8_lossy(stdout).into_owned()
    };

    let err = if stderr.len() > half {
        truncated = true;
        let s = String::from_utf8_lossy(&stderr[..half]);
        format!("{s}\n... truncated ({} bytes total)", stderr.len())
    } else {
        String::from_utf8_lossy(stderr).into_owned()
    };

    (out, err, truncated)
}

/// Assemble the tokio `Command` that executes `command_str` under
/// the active `SandboxPolicy`. Off → a direct host command (the
/// v2.0.0 shape). TierA / TierB → the unprivileged jail via
/// `sandbox::tier_a::build_jailed_tokio_command`, with allow-list
/// paths chosen so bash can start and read libraries but cannot
/// overwrite host files.
///
/// `overlay_handle` is an out-param: when Tier B mounts a
/// fuse-overlayfs, the handle lands here so the caller can
/// inspect `changed_files()` or let `Drop` unmount after the
/// child exits.
fn build_bash_command(
    command_str: &str,
    repo_root: &std::path::Path,
    policy: SandboxPolicy,
    #[allow(unused_variables)] overlay_handle: &mut OverlayHandle,
) -> Result<Command, ToolError> {
    let cwd = repo_root.to_path_buf();

    // Common env shape applied to every variant.
    let apply_env = |mut cmd: Command| -> Command {
        cmd.env_clear()
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("LANG", "C.UTF-8")
            .env("TERM", "dumb");
        cmd
    };

    if policy.is_off() {
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(command_str).current_dir(&cwd);
        return Ok(apply_env(cmd));
    }

    // Linux-only from here on. On non-Linux, degrade to Off with a
    // warning — the sandbox modules return Unsupported anyway.
    #[cfg(not(target_os = "linux"))]
    {
        let _ = repo_root;
        tracing::warn!("AZOTH_SANDBOX set on non-Linux host; degrading to Off");
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(command_str).current_dir(&cwd);
        return Ok(apply_env(cmd));
    }

    #[cfg(target_os = "linux")]
    {
        use crate::sandbox::tier_a::{build_jailed_tokio_command, SpawnOptions};
        use crate::sandbox::{probe_fuse_overlayfs, probe_unprivileged_userns, OverlayWorkspace};

        // v2.1 codex re-review P2: Tier A requires unprivileged
        // CLONE_NEWUSER. Hosts without user-ns support (old
        // kernels, locked-down containers, some CI runners) have
        // the probe return false. Hard-failing every bash call in
        // that environment makes the tool unusable. Tier B already
        // degrades to Tier A when fuse-overlayfs is missing; apply
        // the same pattern for Tier A → Off.
        if !probe_unprivileged_userns() {
            tracing::warn!(
                policy = ?policy,
                "AZOTH_SANDBOX requested but host lacks unprivileged CLONE_NEWUSER; degrading to Off"
            );
            let mut cmd = Command::new("bash");
            cmd.arg("-c").arg(command_str).current_dir(&cwd);
            return Ok(apply_env(cmd));
        }

        // Landlock allow-list. Broad enough for bash + core utilities
        // to start; writes clamp to /tmp + (Tier B) the overlay upper
        // layer. The repo root is readable so `cat` / `rg` / scripts
        // can inspect the tree under Tier A.
        let mut allow_read: Vec<std::path::PathBuf> = vec![
            std::path::PathBuf::from("/bin"),
            std::path::PathBuf::from("/lib"),
            std::path::PathBuf::from("/lib64"),
            std::path::PathBuf::from("/usr"),
            std::path::PathBuf::from("/etc"),
            std::path::PathBuf::from("/proc"),
            std::path::PathBuf::from("/sys"),
            std::path::PathBuf::from("/dev"),
            cwd.clone(),
        ];
        // Narrow `/dev` allow-write down to the handful of device
        // nodes bash genuinely needs for stdio + randomness, rather
        // than granting write access to every device node under
        // `/dev` (PR #14 gemini SECURITY-MEDIUM). Missing paths
        // on a given host are silently dropped by Landlock — the
        // ruleset builder only fails on fs-ops-level errors, not
        // on individual path presence.
        let mut allow_write: Vec<std::path::PathBuf> = vec![
            std::path::PathBuf::from("/tmp"),
            std::path::PathBuf::from("/dev/null"),
            std::path::PathBuf::from("/dev/tty"),
            std::path::PathBuf::from("/dev/urandom"),
            std::path::PathBuf::from("/dev/random"),
            std::path::PathBuf::from("/dev/zero"),
        ];

        // Tier B: mount overlay; bash's cwd becomes the merged view
        // so writes land in the upper layer, not the real repo.
        let final_cwd: std::path::PathBuf = if policy.is_tier_b() {
            if !probe_fuse_overlayfs() {
                tracing::warn!(
                    "AZOTH_SANDBOX=tier_b requested but fuse-overlayfs is not on PATH; degrading to Tier A"
                );
                cwd.clone()
            } else {
                match OverlayWorkspace::mount(&cwd) {
                    Ok(ws) => {
                        let merged = ws.merged.clone();
                        // The overlay's merged dir and its internal
                        // upper must both be writable; the lower is
                        // covered by the repo_root allow_read above.
                        allow_read.push(merged.clone());
                        allow_write.push(merged.clone());
                        *overlay_handle = Some(ws);
                        merged
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "OverlayWorkspace::mount failed; degrading to Tier A"
                        );
                        cwd.clone()
                    }
                }
            }
        } else {
            cwd.clone()
        };

        let opts = SpawnOptions {
            allow_read,
            allow_write,
            // Seccomp stays permissive for bash — the narrow /bin/true
            // allowlist kills the shell on the first clone/wait4.
            // Landlock is the effective enforcement here.
            strict_seccomp: false,
        };
        // v2.1 codex re-review P2: the probe above rules out the
        // most common "host lacks user-ns" case, but jail
        // construction can still fail on kernels without Landlock
        // or with seccompiler bugs. Degrade gracefully rather than
        // turning the tool call into a hard failure.
        match build_jailed_tokio_command("bash", &["-c", command_str], &opts, Some(&final_cwd)) {
            Ok(cmd) => Ok(apply_env(cmd)),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    policy = ?policy,
                    "sandbox jail build failed; degrading to unsandboxed bash for this invocation"
                );
                // Drop any overlay we mounted above — we're about
                // to run against the real repo, so the upper layer
                // would never be staged.
                *overlay_handle = None;
                let mut cmd = Command::new("bash");
                cmd.arg("-c").arg(command_str).current_dir(&cwd);
                Ok(apply_env(cmd))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::ArtifactStore;
    use crate::authority::{Origin, Tainted};
    use crate::execution::{dispatch_tool, ExecutionContext, ToolDispatcher};
    use crate::schemas::{RunId, TurnId};
    use tempfile::tempdir;

    fn ctx_for(root: std::path::PathBuf) -> ExecutionContext {
        let artifacts = ArtifactStore::open(root.join(".azoth/artifacts")).unwrap();
        ExecutionContext::builder(
            RunId::from("run_t".to_string()),
            TurnId::from("t_t".to_string()),
            artifacts,
            root,
        )
        .build()
    }

    #[tokio::test]
    async fn echo_hello() {
        let dir = tempdir().unwrap();
        let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
        let ctx = ctx_for(root);

        let mut disp = ToolDispatcher::new();
        disp.register(BashTool);
        let raw = Tainted::new(Origin::ModelOutput, json!({ "command": "echo hello" }));
        let out = dispatch_tool(&disp, "bash", raw, &ctx).await.unwrap();
        assert_eq!(out["exit_code"], 0);
        assert_eq!(out["stdout"].as_str().unwrap().trim(), "hello");
        assert_eq!(out["timed_out"], false);
    }

    #[tokio::test]
    async fn nonzero_exit() {
        let dir = tempdir().unwrap();
        let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
        let ctx = ctx_for(root);

        let mut disp = ToolDispatcher::new();
        disp.register(BashTool);
        let raw = Tainted::new(Origin::ModelOutput, json!({ "command": "exit 42" }));
        let out = dispatch_tool(&disp, "bash", raw, &ctx).await.unwrap();
        assert_eq!(out["exit_code"], 42);
    }

    #[tokio::test]
    async fn timeout_kills_process() {
        let dir = tempdir().unwrap();
        let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
        let ctx = ctx_for(root);

        let mut disp = ToolDispatcher::new();
        disp.register(BashTool);
        let raw = Tainted::new(
            Origin::ModelOutput,
            json!({ "command": "sleep 300", "timeout_ms": 1000 }),
        );
        let out = dispatch_tool(&disp, "bash", raw, &ctx).await.unwrap();
        assert_eq!(out["timed_out"], true);
        assert!(out["exit_code"].is_null());
    }

    #[tokio::test]
    async fn cancellation_kills_process() {
        let dir = tempdir().unwrap();
        let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
        let ctx = ctx_for(root.clone());
        let token = ctx.cancellation.clone();

        let mut disp = ToolDispatcher::new();
        disp.register(BashTool);
        let raw = Tainted::new(Origin::ModelOutput, json!({ "command": "sleep 300" }));

        let handle = tokio::spawn({
            let disp = std::sync::Arc::new(disp);
            async move {
                let tool = disp.tool("bash").unwrap();
                tool.dispatch(raw, &ctx).await
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        token.cancel();

        let err = handle.await.unwrap().expect_err("should be cancelled");
        match err {
            ToolError::Cancelled => {}
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn runs_in_repo_root() {
        let dir = tempdir().unwrap();
        let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
        tokio::fs::write(root.join("marker.txt"), "found-it")
            .await
            .unwrap();
        let ctx = ctx_for(root);

        let mut disp = ToolDispatcher::new();
        disp.register(BashTool);
        let raw = Tainted::new(Origin::ModelOutput, json!({ "command": "cat marker.txt" }));
        let out = dispatch_tool(&disp, "bash", raw, &ctx).await.unwrap();
        assert_eq!(out["stdout"].as_str().unwrap().trim(), "found-it");
    }

    // ───────────────────────── v2.1 sandbox smoke ─────────────────────────
    //
    // Three tests prove the sandbox wiring actually enforces rather
    // than compiling cleanly and doing nothing:
    //   1. Default policy (AZOTH_SANDBOX unset) writes to the real
    //      repo root. Regression guard against accidentally flipping
    //      the default.
    //   2. Tier A blocks a write to /etc/passwd via Landlock. This
    //      is the load-bearing "sandbox actually enforces" assertion.
    //   3. Tier B isolates writes in the fuse-overlayfs upper layer
    //      so the real repo stays pristine after the tool returns.
    //
    // Tests skip cleanly when the host can't unshare a user
    // namespace (WSL2 without user-ns enabled) — matching the
    // existing `sandbox_tier_a_smoke` gate.

    #[cfg(target_os = "linux")]
    fn sandbox_skip() -> bool {
        use crate::sandbox::probe_unprivileged_userns;
        if std::env::var_os("AZOTH_SKIP_TIER_A").is_some() {
            eprintln!("skip: AZOTH_SKIP_TIER_A set");
            return true;
        }
        if !probe_unprivileged_userns() {
            eprintln!("skip: host lacks unprivileged CLONE_NEWUSER");
            return true;
        }
        false
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bash_default_policy_still_writes_to_repo_root_regression_guard() {
        std::env::remove_var("AZOTH_SANDBOX");

        let dir = tempdir().unwrap();
        let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
        let ctx = ctx_for(root.clone());

        let mut disp = ToolDispatcher::new();
        disp.register(BashTool);
        let raw = Tainted::new(
            Origin::ModelOutput,
            json!({ "command": "echo hello > marker.txt" }),
        );
        let out = dispatch_tool(&disp, "bash", raw, &ctx).await.unwrap();
        assert_eq!(out["exit_code"], 0);
        assert!(
            root.join("marker.txt").exists(),
            "default policy must write to the real repo root"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bash_tier_a_landlock_blocks_write_to_etc_passwd() {
        if sandbox_skip() {
            return;
        }
        std::env::set_var("AZOTH_SANDBOX", "tier_a");

        let dir = tempdir().unwrap();
        let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
        let ctx = ctx_for(root.clone());

        let mut disp = ToolDispatcher::new();
        disp.register(BashTool);
        let raw = Tainted::new(
            Origin::ModelOutput,
            json!({
                "command": "echo pwned > /etc/passwd.azoth_smoke 2>&1; echo done"
            }),
        );
        let out = dispatch_tool(&disp, "bash", raw, &ctx).await.unwrap();
        std::env::remove_var("AZOTH_SANDBOX");

        // Load-bearing assertion: the host file must not exist.
        // The bash may exit 0 because `echo done` follows the
        // failed write, but Landlock must have denied the write.
        assert!(
            !std::path::Path::new("/etc/passwd.azoth_smoke").exists(),
            "sandboxed bash managed to create /etc/passwd.azoth_smoke — landlock not enforcing"
        );
        let combined = format!(
            "{}\n{}",
            out["stdout"].as_str().unwrap_or(""),
            out["stderr"].as_str().unwrap_or("")
        )
        .to_lowercase();
        assert!(
            combined.contains("permission denied")
                || combined.contains("read-only")
                || combined.contains("cannot create"),
            "expected permission-denied flavour in bash output, got {combined:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bash_tier_b_stages_writes_back_to_repo_on_success() {
        // PR #14 codex P1 fix regression guard. Previous semantics
        // let overlay writes DROP silently on scope exit — bash
        // reported success, file never reached the real repo.
        // This test pins the corrected contract: success → write
        // committed, `staged_files` lists it; failure → nothing
        // leaks (covered by `bash_tier_b_discards_writes_on_failure`).
        use crate::sandbox::tier_b::probe_fuse_overlayfs;
        if sandbox_skip() {
            return;
        }
        if !probe_fuse_overlayfs() {
            eprintln!("skip: fuse-overlayfs not on PATH");
            return;
        }
        std::env::set_var("AZOTH_SANDBOX", "tier_b");

        let dir = tempdir().unwrap();
        let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
        tokio::fs::write(root.join("existing.txt"), "original")
            .await
            .unwrap();
        let ctx = ctx_for(root.clone());

        let mut disp = ToolDispatcher::new();
        disp.register(BashTool);
        let raw = Tainted::new(
            Origin::ModelOutput,
            json!({
                "command": "echo sandbox > staged.txt && cat existing.txt"
            }),
        );
        let out = dispatch_tool(&disp, "bash", raw, &ctx).await.unwrap();
        std::env::remove_var("AZOTH_SANDBOX");

        assert_eq!(out["exit_code"], 0, "bash succeeded");
        assert!(
            root.join("staged.txt").exists(),
            "Tier B must stage overlay writes back to real repo on success (codex P1 fix)"
        );
        let contents = std::fs::read_to_string(root.join("staged.txt")).unwrap();
        assert_eq!(contents.trim(), "sandbox");
        // Bash could still read the pre-existing file through the
        // merged view.
        let stdout = out["stdout"].as_str().unwrap_or("");
        assert!(
            stdout.contains("original"),
            "bash should read `existing.txt` through the merged overlay view; got stdout={stdout:?}"
        );
        // staged_files surfaces the commit explicitly so the
        // caller knows what landed.
        let staged = out["staged_files"].as_array().expect("staged_files array");
        assert!(
            staged.iter().any(|v| v == "staged.txt"),
            "expected staged_files to contain 'staged.txt'; got {staged:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bash_tier_b_discards_writes_on_failure() {
        // Complement to the success-stages test: on non-zero
        // exit, the overlay's upper layer is dropped and the
        // real repo stays pristine. That's Tier B's isolation
        // contract — a bad turn leaves no trace.
        use crate::sandbox::tier_b::probe_fuse_overlayfs;
        if sandbox_skip() {
            return;
        }
        if !probe_fuse_overlayfs() {
            eprintln!("skip: fuse-overlayfs not on PATH");
            return;
        }
        std::env::set_var("AZOTH_SANDBOX", "tier_b");

        let dir = tempdir().unwrap();
        let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
        let ctx = ctx_for(root.clone());

        let mut disp = ToolDispatcher::new();
        disp.register(BashTool);
        // Write, then exit non-zero. The write landed in the
        // overlay upper layer, but because exit was non-zero,
        // stage_overlay_back refuses to commit.
        let raw = Tainted::new(
            Origin::ModelOutput,
            json!({
                "command": "echo fail > should_not_leak.txt; exit 17"
            }),
        );
        let out = dispatch_tool(&disp, "bash", raw, &ctx).await.unwrap();
        std::env::remove_var("AZOTH_SANDBOX");

        assert_eq!(out["exit_code"], 17);
        assert!(
            !root.join("should_not_leak.txt").exists(),
            "Tier B must discard overlay writes on failed exit; expected pristine repo"
        );
        let staged = out["staged_files"].as_array();
        // `skip_serializing_if = Vec::is_empty` may drop the
        // field entirely — either None or an empty array both
        // satisfy the contract.
        match staged {
            None => {} // dropped entirely — fine
            Some(arr) => assert!(arr.is_empty(), "no files staged on failure; got {arr:?}"),
        }
    }

    /// Codex round-4 P1 regression guard: `rm` under Tier B
    /// writes a fuse-overlayfs whiteout in the upper layer; the
    /// staging path must detect that and propagate the deletion
    /// to the real repo rather than blindly copying the whiteout
    /// artifact. Without this, the repo stays inconsistent with
    /// bash's exit_code=0 — a silent no-op.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bash_tier_b_propagates_whiteout_deletion_to_real_repo() {
        use crate::sandbox::tier_b::probe_fuse_overlayfs;
        if sandbox_skip() {
            return;
        }
        if !probe_fuse_overlayfs() {
            eprintln!("skip: fuse-overlayfs not on PATH");
            return;
        }
        std::env::set_var("AZOTH_SANDBOX", "tier_b");

        let dir = tempdir().unwrap();
        let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
        // Seed a file in the lower layer (real repo) that bash
        // will delete through the overlay.
        tokio::fs::write(root.join("to_delete.txt"), "goodbye")
            .await
            .unwrap();
        let ctx = ctx_for(root.clone());

        let mut disp = ToolDispatcher::new();
        disp.register(BashTool);
        let raw = Tainted::new(
            Origin::ModelOutput,
            json!({ "command": "rm to_delete.txt && echo gone" }),
        );
        let out = dispatch_tool(&disp, "bash", raw, &ctx).await.unwrap();
        std::env::remove_var("AZOTH_SANDBOX");

        assert_eq!(out["exit_code"], 0, "bash rm succeeded");
        assert!(
            !root.join("to_delete.txt").exists(),
            "Tier B must propagate the deletion to the real repo; got file still present"
        );
        let removed = out["removed_files"]
            .as_array()
            .expect("removed_files array in output");
        assert!(
            removed.iter().any(|v| v == "to_delete.txt"),
            "removed_files must list the deleted path; got {removed:?}"
        );
        // And it should NOT show up in staged_files — deletions
        // and writes surface through separate fields.
        let staged = out["staged_files"].as_array();
        match staged {
            None => {}
            Some(arr) => {
                assert!(
                    !arr.iter().any(|v| v == "to_delete.txt"),
                    "deletion must not appear in staged_files; got {arr:?}"
                );
            }
        }
    }

    /// Codex round-5 P1 regression guard: `ln -s target link`
    /// under Tier B must reproduce a SYMLINK at the real repo,
    /// not a regular file containing the target's bytes. Prior
    /// `std::fs::copy` path dereferenced the link.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bash_tier_b_preserves_symlinks_in_stage_back() {
        use crate::sandbox::tier_b::probe_fuse_overlayfs;
        if sandbox_skip() {
            return;
        }
        if !probe_fuse_overlayfs() {
            eprintln!("skip: fuse-overlayfs not on PATH");
            return;
        }
        std::env::set_var("AZOTH_SANDBOX", "tier_b");

        let dir = tempdir().unwrap();
        let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
        // Seed a file the symlink will point at.
        tokio::fs::write(root.join("target.txt"), "real contents")
            .await
            .unwrap();
        let ctx = ctx_for(root.clone());

        let mut disp = ToolDispatcher::new();
        disp.register(BashTool);
        let raw = Tainted::new(
            Origin::ModelOutput,
            json!({ "command": "ln -s target.txt link.txt" }),
        );
        let out = dispatch_tool(&disp, "bash", raw, &ctx).await.unwrap();
        std::env::remove_var("AZOTH_SANDBOX");

        assert_eq!(out["exit_code"], 0, "bash ln -s succeeded");
        // Real repo must now have a symlink at link.txt, not a
        // regular file. `symlink_metadata` doesn't follow.
        let link_meta = std::fs::symlink_metadata(root.join("link.txt"))
            .expect("link.txt should exist in real repo after Tier B stage-back");
        assert!(
            link_meta.file_type().is_symlink(),
            "link.txt must be a symlink; got file_type={:?}",
            link_meta.file_type()
        );
        let resolved = std::fs::read_link(root.join("link.txt")).unwrap();
        assert_eq!(
            resolved.as_os_str(),
            "target.txt",
            "symlink target must be preserved verbatim"
        );
        // staged_files should include the link.
        let staged = out["staged_files"].as_array().expect("staged_files array");
        assert!(
            staged.iter().any(|v| v == "link.txt"),
            "link.txt must appear in staged_files; got {staged:?}"
        );
    }

    /// Codex round-6 P1 SECURITY regression guard: a Tier-B
    /// `ln -s /etc leak` used to make `OverlayWorkspace::changed_files()`
    /// descend into /etc (because `path.is_dir()` followed the
    /// symlink), and `stage_overlay_back` then copied `/etc/passwd`
    /// etc. into the real repo. This test exercises the attack:
    /// bash creates a symlink to a host directory under Tier B,
    /// then attempts to force the symlink's contents into the
    /// repo. After the fix, the symlink itself is recreated (ok,
    /// that's fine — it's a pointer, not a leak) but NO
    /// host-file contents materialise under the symlink's path
    /// in the real repo. The canonicalize-boundary check also
    /// refuses to stage anything whose resolved source escapes
    /// `ws.upper`.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bash_tier_b_blocks_symlink_dir_traversal_to_host_files() {
        use crate::sandbox::tier_b::probe_fuse_overlayfs;
        if sandbox_skip() {
            return;
        }
        if !probe_fuse_overlayfs() {
            eprintln!("skip: fuse-overlayfs not on PATH");
            return;
        }
        std::env::set_var("AZOTH_SANDBOX", "tier_b");

        let dir = tempdir().unwrap();
        let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
        let ctx = ctx_for(root.clone());

        let mut disp = ToolDispatcher::new();
        disp.register(BashTool);
        // Point a symlink at /etc — a readable host directory.
        // Pre-fix behaviour: `changed_files()` recurses into /etc,
        // stage_overlay_back copies /etc/passwd etc. into repo.
        // Post-fix: walker records the symlink itself, never
        // descends; boundary check in stage_overlay_back also
        // refuses any entry that resolves outside upper.
        let raw = Tainted::new(
            Origin::ModelOutput,
            json!({ "command": "ln -s /etc leak && ls leak/passwd >/dev/null 2>&1 || true" }),
        );
        let out = dispatch_tool(&disp, "bash", raw, &ctx).await.unwrap();
        std::env::remove_var("AZOTH_SANDBOX");

        assert_eq!(out["exit_code"], 0, "bash ln -s succeeded");
        // Load-bearing assertion: the symlink may exist in the
        // real repo (as a dangling or valid pointer), but its
        // "contents" must not — i.e., there must be no regular
        // file at `repo_root/leak/passwd`.
        let leaked = root.join("leak/passwd");
        assert!(
            !leaked.exists()
                || std::fs::symlink_metadata(&leaked)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false),
            "host file materialised via symlink-dir traversal: {}",
            leaked.display()
        );
        let leaked_file = std::fs::read_to_string(&leaked).ok();
        // Either we can't read anything (link traverses into a
        // repo path that doesn't exist) or if we can, it's NOT
        // real /etc/passwd contents.
        if let Some(contents) = leaked_file {
            // /etc/passwd invariably starts with "root:" — reject.
            assert!(
                !contents.starts_with("root:"),
                "leaked /etc/passwd contents into repo: {contents:?}"
            );
        }
    }

    /// Codex round-7 P1 regression guard: `mkfifo pipe` under
    /// Tier B used to hang \[stage_overlay_back\] indefinitely —
    /// round-6's walker change made FIFOs/sockets flow into
    /// `changed_files()`, and `std::fs::copy` on a FIFO opens
    /// it for reading and blocks waiting for a writer. Tool
    /// reported success by bash but execute() never returned.
    ///
    /// Post-fix: non-regular-file entries are skipped with a
    /// tracing::warn. This test proves the tool returns
    /// promptly (under a generous timeout) and the FIFO does
    /// NOT appear in staged_files. Test wall-clock is bounded
    /// by tokio::time::timeout so a regression cannot hang CI.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bash_tier_b_skips_fifo_entries_without_hanging() {
        use crate::sandbox::tier_b::probe_fuse_overlayfs;
        if sandbox_skip() {
            return;
        }
        if !probe_fuse_overlayfs() {
            eprintln!("skip: fuse-overlayfs not on PATH");
            return;
        }
        std::env::set_var("AZOTH_SANDBOX", "tier_b");

        let dir = tempdir().unwrap();
        let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
        let ctx = ctx_for(root.clone());

        let mut disp = ToolDispatcher::new();
        disp.register(BashTool);
        // Create a FIFO + a regular file. Pre-fix: hangs on
        // the FIFO copy. Post-fix: FIFO skipped with warn,
        // regular file staged normally.
        let raw = Tainted::new(
            Origin::ModelOutput,
            json!({ "command": "mkfifo pipe && echo written > real.txt" }),
        );
        // Hard-cap the dispatch; a regression would block
        // here on the FIFO copy. 10s is generous for the
        // real stage-back work but short enough to fail fast
        // on a hang.
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            dispatch_tool(&disp, "bash", raw, &ctx),
        )
        .await
        .expect("stage-back hung — did we regress FIFO skip?")
        .unwrap();
        std::env::remove_var("AZOTH_SANDBOX");

        assert_eq!(out["exit_code"], 0);
        let staged = out["staged_files"].as_array().expect("staged_files");
        assert!(
            staged.iter().any(|v| v == "real.txt"),
            "real.txt must stage; got {staged:?}"
        );
        assert!(
            !staged.iter().any(|v| v == "pipe"),
            "FIFO must NOT appear in staged_files (skipped); got {staged:?}"
        );
        // And no regular file at repo_root/pipe.
        let pipe_at_repo = root.join("pipe");
        if pipe_at_repo.exists() {
            let ft = std::fs::symlink_metadata(&pipe_at_repo)
                .unwrap()
                .file_type();
            assert!(
                !ft.is_file(),
                "FIFO was materialised as a regular file at {}",
                pipe_at_repo.display()
            );
        }
    }
}
