//! `repo_search` — literal substring scan of the repo root.
//!
//! A stand-in for the ripgrep-backed LexicalRetrieval; functional enough for
//! the ToolDispatcher smoke test without pulling in an external binary.

use crate::execution::{ExecutionContext, Tool, ToolError};
use crate::schemas::EffectClass;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct RepoSearchInput {
    pub q: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct RepoSearchOutput {
    pub matches: Vec<Hit>,
}

#[derive(Debug, Serialize)]
pub struct Hit {
    pub path: String,
    pub line: usize,
    pub excerpt: String,
}

pub struct RepoSearchTool;

#[async_trait]
impl Tool for RepoSearchTool {
    type Input = RepoSearchInput;
    type Output = RepoSearchOutput;

    fn name(&self) -> &'static str {
        // Must match `^[a-zA-Z0-9_-]{1,128}$` — Anthropic Messages API
        // rejects any other shape. `tool_names_satisfy_provider_regex`
        // integration test pins this at CI time.
        "repo_search"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "q": {"type": "string"},
                "limit": {"type": "integer", "minimum": 1}
            },
            "required": ["q"]
        })
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::Observe
    }

    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ExecutionContext,
    ) -> Result<Self::Output, ToolError> {
        let limit = input.limit.unwrap_or(50);
        let mut matches = Vec::new();
        walk(&ctx.repo_root, &input.q, limit, &mut matches)
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        Ok(RepoSearchOutput { matches })
    }
}

fn walk(root: &Path, needle: &str, limit: usize, out: &mut Vec<Hit>) -> std::io::Result<()> {
    if out.len() >= limit || !root.exists() {
        return Ok(());
    }
    let Ok(entries) = fs::read_dir(root) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        if out.len() >= limit {
            break;
        }
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue;
        }
        if path.is_dir() {
            walk(&path, needle, limit, out)?;
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        for (idx, line) in content.lines().enumerate() {
            if !line.contains(needle) {
                continue;
            }
            out.push(Hit {
                path: path.display().to_string(),
                line: idx + 1,
                excerpt: line.trim().chars().take(200).collect(),
            });
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(())
}
