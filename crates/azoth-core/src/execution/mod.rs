//! Tool dispatcher. The blanket `ErasedTool` impl is the taint-gate seam —
//! individual tool implementations never see raw JSON input.

pub mod clock;
mod context;
mod dispatcher;

pub use clock::{system_clock, Clock, FrozenClock, SystemClock, VirtualClock};
pub use context::{CancellationToken, ExecutionContext, ExecutionContextBuilder};
pub use dispatcher::{dispatch_tool, ErasedTool, Tool, ToolDispatcher, ToolError};
