//! `bash` — run a shell command inside the repo root. The process inherits
//! only a sanitized environment. Output is capped to prevent context
//! blowout. Respects the execution context's cancellation token.

use crate::execution::{ExecutionContext, Tool, ToolError};
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

        let mut child = Command::new("bash")
            .arg("-c")
            .arg(&input.command)
            .current_dir(&ctx.repo_root)
            .env_clear()
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("LANG", "C.UTF-8")
            .env("TERM", "dumb")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::ArtifactStore;
    use crate::authority::{Origin, Tainted};
    use crate::execution::{dispatch_tool, CancellationToken, ExecutionContext, ToolDispatcher};
    use crate::schemas::{RunId, TurnId};
    use tempfile::tempdir;

    fn ctx_for(root: std::path::PathBuf) -> ExecutionContext {
        let artifacts = ArtifactStore::open(root.join(".azoth/artifacts")).unwrap();
        ExecutionContext {
            run_id: RunId::from("run_t".to_string()),
            turn_id: TurnId::from("t_t".to_string()),
            artifacts,
            cancellation: CancellationToken::new(),
            repo_root: root,
        }
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
}
