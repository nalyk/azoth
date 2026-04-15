//! Tool dispatcher. The blanket `ErasedTool` impl is the taint-gate seam —
//! individual tool implementations never see raw JSON input.

mod context;
mod dispatcher;

pub use context::{CancellationToken, ExecutionContext};
pub use dispatcher::{dispatch_tool, ErasedTool, Tool, ToolDispatcher, ToolError};
