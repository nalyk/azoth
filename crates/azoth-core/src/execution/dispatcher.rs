//! The taint gate. `Tool` works with typed input structs; the blanket
//! `ErasedTool` impl extracts `Tainted<Value>` once, at the seam, before
//! calling `Tool::execute`.

use super::context::ExecutionContext;
use crate::authority::{ExtractionError, Extractor, JsonExtractor, Origin, Tainted};
use crate::schemas::EffectClass;
use async_trait::async_trait;
use futures::future::BoxFuture;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    Unknown(String),
    #[error("extraction: {0}")]
    Extraction(#[from] ExtractionError),
    #[error("serialization: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("sandbox denied: {0}")]
    SandboxDenied(String),
    #[error("cancelled")]
    Cancelled,
    #[error("tool failed: {0}")]
    Failed(String),
}

/// The typed tool trait. Concrete tools implement this; they never see raw
/// JSON. The dispatcher extracts into `Self::Input` first.
#[async_trait]
pub trait Tool: Send + Sync {
    type Input: serde::de::DeserializeOwned + Send;
    type Output: serde::Serialize + Send;

    fn name(&self) -> &'static str;
    fn schema(&self) -> Value;
    fn effect_class(&self) -> EffectClass;
    /// Which `Origin`s the dispatcher will accept for this tool's input.
    /// Most tools accept only `ModelOutput`.
    fn permitted_origins(&self) -> &'static [Origin] {
        &[Origin::ModelOutput]
    }

    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ExecutionContext,
    ) -> Result<Self::Output, ToolError>;
}

/// Erased form held in the dispatcher's registry. Implemented by a blanket
/// impl over every `Tool` so tool authors cannot bypass the taint gate.
pub trait ErasedTool: Send + Sync {
    fn name(&self) -> &'static str;
    fn schema(&self) -> Value;
    fn effect_class(&self) -> EffectClass;
    fn dispatch<'a>(
        &'a self,
        raw: Tainted<Value>,
        ctx: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<Value, ToolError>>;
}

impl<T: Tool + 'static> ErasedTool for T {
    fn name(&self) -> &'static str {
        Tool::name(self)
    }
    fn schema(&self) -> Value {
        Tool::schema(self)
    }
    fn effect_class(&self) -> EffectClass {
        Tool::effect_class(self)
    }
    fn dispatch<'a>(
        &'a self,
        raw: Tainted<Value>,
        ctx: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<Value, ToolError>> {
        Box::pin(async move {
            // Taint gate: Tool::permitted_origins() drives the JsonExtractor's
            // allowlist. We can't easily pass that into `JsonExtractor::new`
            // since it wants a 'static slice, so we check manually here.
            let permitted = T::permitted_origins(self);
            if !permitted.contains(&raw.origin()) {
                return Err(ToolError::Extraction(ExtractionError::OriginNotPermitted(
                    raw.origin(),
                    Tool::name(self),
                )));
            }
            let input: T::Input = {
                let ex: JsonExtractor<T::Input> = JsonExtractor::new(permitted);
                ex.extract(raw)?
            };
            let out = self.execute(input, ctx).await?;
            Ok(serde_json::to_value(out)?)
        })
    }
}

#[derive(Default)]
pub struct ToolDispatcher {
    tools: HashMap<String, Arc<dyn ErasedTool>>,
}

impl ToolDispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<T: ErasedTool + 'static>(&mut self, tool: T) {
        let name = tool.name().to_string();
        self.tools.insert(name, Arc::new(tool));
    }

    pub fn tool(&self, name: &str) -> Option<Arc<dyn ErasedTool>> {
        self.tools.get(name).cloned()
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    pub fn schemas(&self) -> Vec<crate::schemas::ToolDefinition> {
        self.tools
            .values()
            .map(|t| crate::schemas::ToolDefinition {
                name: t.name().to_string(),
                description: String::new(),
                input_schema: t.schema(),
            })
            .collect()
    }
}

/// Convenience top-level dispatch helper. The caller is responsible for
/// taint-wrapping the raw input with the correct `Origin`.
pub async fn dispatch_tool(
    dispatcher: &ToolDispatcher,
    tool_name: &str,
    raw: Tainted<Value>,
    ctx: &ExecutionContext,
) -> Result<Value, ToolError> {
    let tool = dispatcher
        .tool(tool_name)
        .ok_or_else(|| ToolError::Unknown(tool_name.to_string()))?;
    if ctx.cancelled() {
        return Err(ToolError::Cancelled);
    }
    tool.dispatch(raw, ctx).await
}
