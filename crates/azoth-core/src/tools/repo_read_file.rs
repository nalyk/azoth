//! `repo.read_file` — read a single file from the repo root. Supports
//! optional line-range slicing. Same path-traversal guard as `fs.write`.

use crate::execution::{ExecutionContext, Tool, ToolError};
use crate::schemas::EffectClass;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub struct RepoReadFileTool;

#[derive(Debug, Deserialize)]
pub struct RepoReadFileInput {
    pub path: String,
    #[serde(default)]
    pub start_line: Option<usize>,
    #[serde(default)]
    pub end_line: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct RepoReadFileOutput {
    pub path: String,
    pub content: String,
    pub total_lines: usize,
    pub range: Option<[usize; 2]>,
}

#[async_trait]
impl Tool for RepoReadFileTool {
    type Input = RepoReadFileInput;
    type Output = RepoReadFileOutput;

    fn name(&self) -> &'static str {
        "repo.read_file"
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::Observe
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "start_line": { "type": "integer", "minimum": 1 },
                "end_line": { "type": "integer", "minimum": 1 }
            },
            "required": ["path"]
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
        let canon_target = tokio::fs::canonicalize(&target)
            .await
            .map_err(|e| ToolError::Failed(format!("file not found: {e}")))?;

        if !canon_target.starts_with(&canon_root) {
            return Err(ToolError::Failed("path escapes repo root".into()));
        }

        let raw = tokio::fs::read_to_string(&canon_target)
            .await
            .map_err(|e| ToolError::Failed(format!("read: {e}")))?;

        let lines: Vec<&str> = raw.lines().collect();
        let total_lines = lines.len();

        let (content, range) = match (input.start_line, input.end_line) {
            (Some(s), Some(e)) => {
                let s = s.saturating_sub(1).min(total_lines);
                let e = e.min(total_lines);
                let slice = &lines[s..e];
                (numbered(slice, s), Some([s + 1, e]))
            }
            (Some(s), None) => {
                let s = s.saturating_sub(1).min(total_lines);
                let slice = &lines[s..];
                (numbered(slice, s), Some([s + 1, total_lines]))
            }
            _ => (numbered(&lines, 0), None),
        };

        Ok(RepoReadFileOutput {
            path: input.path,
            content,
            total_lines,
            range,
        })
    }
}

fn numbered(lines: &[&str], offset: usize) -> String {
    let mut buf = String::new();
    for (i, line) in lines.iter().enumerate() {
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(&format!("{:>5}\t{}", offset + i + 1, line));
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::ArtifactStore;
    use crate::authority::{Origin, Tainted};
    use crate::execution::{dispatch_tool, CancellationToken, ExecutionContext, ToolDispatcher};
    use crate::schemas::{RunId, TurnId};
    use tempfile::tempdir;
    use tokio::fs;

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
    async fn reads_whole_file() {
        let dir = tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).await.unwrap();
        fs::write(root.join("hello.txt"), "alpha\nbeta\ngamma\n")
            .await
            .unwrap();
        let ctx = ctx_for(root);

        let mut disp = ToolDispatcher::new();
        disp.register(RepoReadFileTool);
        let raw = Tainted::new(Origin::ModelOutput, json!({ "path": "hello.txt" }));
        let out = dispatch_tool(&disp, "repo.read_file", raw, &ctx)
            .await
            .unwrap();
        assert_eq!(out["total_lines"], 3);
        let content = out["content"].as_str().unwrap();
        assert!(content.contains("alpha"));
        assert!(content.contains("gamma"));
    }

    #[tokio::test]
    async fn reads_line_range() {
        let dir = tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).await.unwrap();
        let body = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(root.join("big.txt"), &body).await.unwrap();
        let ctx = ctx_for(root);

        let mut disp = ToolDispatcher::new();
        disp.register(RepoReadFileTool);
        let raw = Tainted::new(
            Origin::ModelOutput,
            json!({ "path": "big.txt", "start_line": 5, "end_line": 8 }),
        );
        let out = dispatch_tool(&disp, "repo.read_file", raw, &ctx)
            .await
            .unwrap();
        let content = out["content"].as_str().unwrap();
        assert!(content.contains("line 5"));
        assert!(content.contains("line 8"));
        assert!(!content.contains("line 4"));
        assert!(!content.contains("line 9"));
        assert_eq!(out["total_lines"], 20);
    }

    #[tokio::test]
    async fn rejects_path_traversal() {
        let dir = tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).await.unwrap();
        fs::write(root.join("ok.txt"), "hi").await.unwrap();
        let ctx = ctx_for(root);

        let mut disp = ToolDispatcher::new();
        disp.register(RepoReadFileTool);
        let raw = Tainted::new(Origin::ModelOutput, json!({ "path": "../../etc/passwd" }));
        let err = dispatch_tool(&disp, "repo.read_file", raw, &ctx)
            .await
            .expect_err("traversal must fail");
        match err {
            ToolError::Failed(msg) => assert!(
                msg.contains("escapes repo root") || msg.contains("not found"),
                "{msg}"
            ),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
