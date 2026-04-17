//! `fs.write` — the canonical `ApplyLocal` tool. Writes bytes to a path
//! inside the repo root. Refuses path traversal by canonicalizing the
//! parent directory and asserting it stays under the canonical repo root.

use crate::authority::Origin;
use crate::execution::{ExecutionContext, Tool, ToolError};
use crate::schemas::EffectClass;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub struct FsWriteTool;

#[derive(Debug, Deserialize)]
pub struct FsWriteInput {
    pub path: String,
    pub contents: String,
}

#[derive(Debug, Serialize)]
pub struct FsWriteOutput {
    pub path: String,
    pub bytes_written: u64,
}

#[async_trait]
impl Tool for FsWriteTool {
    type Input = FsWriteInput;
    type Output = FsWriteOutput;

    fn name(&self) -> &'static str {
        "fs.write"
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::ApplyLocal
    }

    fn permitted_origins(&self) -> &'static [Origin] {
        &[Origin::ModelOutput]
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "contents": { "type": "string" }
            },
            "required": ["path", "contents"]
        })
    }

    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ExecutionContext,
    ) -> Result<Self::Output, ToolError> {
        let target = ctx.repo_root.join(&input.path);
        let canon_root = tokio::fs::canonicalize(&ctx.repo_root)
            .await
            .map_err(|e| ToolError::Failed(format!("canonicalize root: {e}")))?;

        // Materialize the parent directory before canonicalizing it — the file
        // itself may not exist yet, so we can't canonicalize `target` directly.
        let parent = target
            .parent()
            .ok_or_else(|| ToolError::Failed("path has no parent".into()))?;
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| ToolError::Failed(format!("mkdir: {e}")))?;

        let parent_canon = tokio::fs::canonicalize(parent)
            .await
            .map_err(|e| ToolError::Failed(format!("canonicalize parent: {e}")))?;
        if !parent_canon.starts_with(&canon_root) {
            return Err(ToolError::Failed("path escapes repo root".into()));
        }

        let file_name = target
            .file_name()
            .ok_or_else(|| ToolError::Failed("path has no file name".into()))?;
        let final_path = parent_canon.join(file_name);

        tokio::fs::write(&final_path, input.contents.as_bytes())
            .await
            .map_err(|e| ToolError::Failed(format!("write: {e}")))?;

        Ok(FsWriteOutput {
            path: input.path,
            bytes_written: input.contents.len() as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::ArtifactStore;
    use crate::authority::Tainted;
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
    async fn writes_file_inside_repo_root() {
        let dir = tempdir().unwrap();
        let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
        let ctx = ctx_for(root.clone());

        let mut disp = ToolDispatcher::new();
        disp.register(FsWriteTool);
        let raw = Tainted::new(
            Origin::ModelOutput,
            json!({ "path": "sub/hello.txt", "contents": "hi from fs.write" }),
        );
        let out = dispatch_tool(&disp, "fs.write", raw, &ctx).await.unwrap();
        assert_eq!(out["bytes_written"], 16);
        let body = tokio::fs::read_to_string(root.join("sub/hello.txt"))
            .await
            .unwrap();
        assert_eq!(body, "hi from fs.write");
    }

    #[tokio::test]
    async fn rejects_path_traversal() {
        let dir = tempdir().unwrap();
        let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
        let ctx = ctx_for(root);

        let mut disp = ToolDispatcher::new();
        disp.register(FsWriteTool);
        let raw = Tainted::new(
            Origin::ModelOutput,
            json!({ "path": "../escape.txt", "contents": "nope" }),
        );
        let err = dispatch_tool(&disp, "fs.write", raw, &ctx)
            .await
            .expect_err("traversal must be rejected");
        match err {
            ToolError::Failed(msg) => assert!(msg.contains("escapes repo root"), "{msg}"),
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
