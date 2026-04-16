//! `repo.read_spans` — read named line ranges from one or more files.
//! Each span is a (path, start_line, end_line) triple. Useful for reading
//! specific functions or regions without pulling entire files into context.

use crate::execution::{ExecutionContext, Tool, ToolError};
use crate::schemas::EffectClass;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub struct RepoReadSpansTool;

#[derive(Debug, Deserialize)]
pub struct Span {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Debug, Deserialize)]
pub struct RepoReadSpansInput {
    pub spans: Vec<Span>,
}

#[derive(Debug, Serialize)]
pub struct SpanResult {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct RepoReadSpansOutput {
    pub results: Vec<SpanResult>,
}

#[async_trait]
impl Tool for RepoReadSpansTool {
    type Input = RepoReadSpansInput;
    type Output = RepoReadSpansOutput;

    fn name(&self) -> &'static str {
        "repo.read_spans"
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::Observe
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "spans": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" },
                            "start_line": { "type": "integer", "minimum": 1 },
                            "end_line": { "type": "integer", "minimum": 1 }
                        },
                        "required": ["path", "start_line", "end_line"]
                    },
                    "minItems": 1,
                    "maxItems": 20
                }
            },
            "required": ["spans"]
        })
    }

    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ExecutionContext,
    ) -> Result<Self::Output, ToolError> {
        if input.spans.is_empty() {
            return Err(ToolError::Failed("spans array is empty".into()));
        }
        if input.spans.len() > 20 {
            return Err(ToolError::Failed("max 20 spans per call".into()));
        }

        let canon_root = tokio::fs::canonicalize(&ctx.repo_root)
            .await
            .map_err(|e| ToolError::Failed(format!("canonicalize root: {e}")))?;

        let mut results = Vec::with_capacity(input.spans.len());

        for span in &input.spans {
            if ctx.cancelled() {
                return Err(ToolError::Cancelled);
            }

            let target = ctx.repo_root.join(&span.path);
            let canon_target = tokio::fs::canonicalize(&target)
                .await
                .map_err(|e| ToolError::Failed(format!("{}: {e}", span.path)))?;

            if !canon_target.starts_with(&canon_root) {
                return Err(ToolError::Failed(format!(
                    "{}: path escapes repo root",
                    span.path
                )));
            }

            let raw = tokio::fs::read_to_string(&canon_target)
                .await
                .map_err(|e| ToolError::Failed(format!("{}: {e}", span.path)))?;

            let lines: Vec<&str> = raw.lines().collect();
            let total = lines.len();
            let s = span.start_line.saturating_sub(1).min(total);
            let e = span.end_line.min(total);
            let slice = &lines[s..e];

            let mut buf = String::new();
            for (i, line) in slice.iter().enumerate() {
                if !buf.is_empty() {
                    buf.push('\n');
                }
                buf.push_str(&format!("{:>5}\t{}", s + i + 1, line));
            }

            results.push(SpanResult {
                path: span.path.clone(),
                start_line: s + 1,
                end_line: e,
                content: buf,
            });
        }

        Ok(RepoReadSpansOutput { results })
    }
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
    async fn reads_multiple_spans() {
        let dir = tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).await.unwrap();

        let body_a = (1..=10)
            .map(|i| format!("a-line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let body_b = (1..=5)
            .map(|i| format!("b-line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(root.join("a.txt"), &body_a).await.unwrap();
        fs::write(root.join("b.txt"), &body_b).await.unwrap();

        let ctx = ctx_for(root);
        let mut disp = ToolDispatcher::new();
        disp.register(RepoReadSpansTool);

        let raw = Tainted::new(
            Origin::ModelOutput,
            json!({
                "spans": [
                    { "path": "a.txt", "start_line": 3, "end_line": 5 },
                    { "path": "b.txt", "start_line": 1, "end_line": 2 }
                ]
            }),
        );
        let out = dispatch_tool(&disp, "repo.read_spans", raw, &ctx)
            .await
            .unwrap();
        let results = out["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);

        let first = &results[0];
        let content = first["content"].as_str().unwrap();
        assert!(content.contains("a-line-3"));
        assert!(content.contains("a-line-5"));
        assert!(!content.contains("a-line-2"));

        let second = &results[1];
        let content = second["content"].as_str().unwrap();
        assert!(content.contains("b-line-1"));
        assert!(content.contains("b-line-2"));
        assert!(!content.contains("b-line-3"));
    }

    #[tokio::test]
    async fn rejects_traversal_in_span() {
        let dir = tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).await.unwrap();
        fs::write(root.join("ok.txt"), "hi\nthere\n").await.unwrap();
        let ctx = ctx_for(root);

        let mut disp = ToolDispatcher::new();
        disp.register(RepoReadSpansTool);
        let raw = Tainted::new(
            Origin::ModelOutput,
            json!({
                "spans": [
                    { "path": "../../etc/passwd", "start_line": 1, "end_line": 5 }
                ]
            }),
        );
        let err = dispatch_tool(&disp, "repo.read_spans", raw, &ctx)
            .await
            .expect_err("traversal must fail");
        match err {
            ToolError::Failed(msg) => assert!(
                msg.contains("escapes repo root") || msg.contains("etc/passwd"),
                "{msg}"
            ),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
