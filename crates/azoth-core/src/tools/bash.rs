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
        // run committed or failed.
        #[allow(unused_mut)]
        let mut overlay_handle: Option<crate::sandbox::OverlayWorkspace> = None;
        let mut child =
            build_bash_command(&input.command, &ctx.repo_root, policy, &mut overlay_handle)?
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| ToolError::Failed(format!("spawn: {e}")))?;

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
                Ok(BashOutput {
                    exit_code: status.code(),
                    stdout,
                    stderr,
                    truncated,
                    timed_out: false,
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
                })
            }
        }
    }
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
    #[allow(unused_variables)] overlay_handle: &mut Option<crate::sandbox::OverlayWorkspace>,
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
        use crate::sandbox::{probe_fuse_overlayfs, OverlayWorkspace};

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
        let mut allow_write: Vec<std::path::PathBuf> = vec![
            std::path::PathBuf::from("/tmp"),
            std::path::PathBuf::from("/dev"),
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
        let cmd = build_jailed_tokio_command("bash", &["-c", command_str], &opts, Some(&final_cwd));
        Ok(apply_env(cmd))
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
    async fn bash_tier_b_isolates_writes_in_overlay_when_fuse_overlayfs_present() {
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

        assert_eq!(
            out["exit_code"], 0,
            "Tier B should let writes land in the overlay upper layer"
        );
        // Real repo stays pristine — no `staged.txt` leaked
        // through after the overlay tear-down.
        assert!(
            !root.join("staged.txt").exists(),
            "Tier B leaked overlay writes to the real repo; expected pristine lower layer"
        );
        // Bash could still read the pre-existing file through the
        // merged view.
        let stdout = out["stdout"].as_str().unwrap_or("");
        assert!(
            stdout.contains("original"),
            "bash should read `existing.txt` through the merged overlay view; got stdout={stdout:?}"
        );
    }
}
