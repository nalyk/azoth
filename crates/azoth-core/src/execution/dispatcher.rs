//! The taint gate. `Tool` works with typed input structs; the blanket
//! `ErasedTool` impl extracts `Tainted<Value>` once, at the seam, before
//! calling `Tool::execute`.

use super::context::ExecutionContext;
use crate::authority::{ExtractionError, Extractor, JsonExtractor, Origin, Tainted};
use crate::sandbox::sandbox_for;
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

    /// Per-invocation effect class refinement for budget accounting.
    /// Default `None` means "use the static `effect_class()`". Tools may
    /// inspect the raw input and downgrade a worst-case static class to
    /// something cheaper for a specific shape — e.g. `BashTool` downgrades
    /// read-only argv (`grep foo src/`) from `ApplyLocal` to `Observe`.
    ///
    /// This return value drives BUDGET and AUTHORITY decisions. The
    /// sandbox tier still selects from the static `effect_class()` so a
    /// mis-classified "observe" invocation cannot escape the worst-case
    /// jail.
    fn effect_class_for(&self, _raw: &Value) -> Option<EffectClass> {
        None
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
    /// Per-invocation refinement. No default — every `ErasedTool` must
    /// route. The blanket impl below forwards to `Tool::effect_class_for`.
    fn effect_class_for(&self, raw: &Value) -> Option<EffectClass>;
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
    fn effect_class_for(&self, raw: &Value) -> Option<EffectClass> {
        <T as Tool>::effect_class_for(self, raw)
    }
    fn dispatch<'a>(
        &'a self,
        raw: Tainted<Value>,
        ctx: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<Value, ToolError>> {
        Box::pin(async move {
            // Sprint 7.5: sandbox gate. Ask the sandbox layer to
            // prepare for the tool's effect class. Tier A/B return
            // Ok (prepare is a no-op for in-process dispatch today —
            // `spawn_jailed` for subprocess isolation is v2.1+
            // scope). Tier C/D return `EffectNotAvailable` as
            // documented by the plan. Either way this puts the
            // seam in place so future tier-B exec-under-sandbox
            // wiring has a call site to hook into, and it prevents
            // ApplyRemote* / ApplyIrreversible tools from silently
            // executing past the documented v2 scope fence.
            //
            // Opt-out: `AZOTH_SANDBOX=off` skips the check — dev-
            // only escape hatch for hosts where the sandbox layer
            // can't initialise. Default is on.
            let ec = Tool::effect_class(self);
            let sandbox_disabled = matches!(
                std::env::var("AZOTH_SANDBOX").as_deref(),
                Ok("off") | Ok("0") | Ok("false")
            );
            if !sandbox_disabled {
                if let Err(e) = sandbox_for(ec).and_then(|s| s.prepare()) {
                    tracing::warn!(
                        tool = Tool::name(self),
                        effect_class = ?ec,
                        error = %e,
                        "sandbox denied"
                    );
                    return Err(ToolError::SandboxDenied(e.to_string()));
                }
                tracing::debug!(
                    tool = Tool::name(self),
                    effect_class = ?ec,
                    "sandbox prepared"
                );
            }

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
        assert!(
            is_valid_provider_tool_name(&name),
            "tool name {name:?} violates the provider tool-name regex \
             ^[a-zA-Z0-9_-]{{1,128}}$ (Anthropic Messages API). Rename \
             to use only ASCII letters, digits, underscore, or hyphen."
        );
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

/// Checks that `name` satisfies Anthropic Messages API's tool-name
/// regex `^[a-zA-Z0-9_-]{1,128}$`. Enforced by `ToolDispatcher::register`
/// so a built-in tool with a dotted / special-char name (which the
/// Anthropic API rejects as `invalid_request_error`) cannot reach
/// production — the worker crashes at startup instead of 400-ing on
/// the first live request.
///
/// The same regex is the narrowest common denominator across provider
/// tool-name validators we've encountered; OpenAI/OpenRouter are
/// permissive, so satisfying Anthropic covers the rest.
pub(crate) fn is_valid_provider_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
