//! v1 built-in tools. Every tool is a concrete `Tool` impl with a typed
//! input struct.

// `bash` is public because its `classifier` submodule is part of the
// public API surface — external crates and integration tests invoke
// `classify_bash_command` to verify policy-layer budget
// classification without paying the cost of building and dispatching
// the real tool. Other tools stay `mod` — open them only when they
// grow a similar hook.
pub mod bash;
mod clock;
mod fs_write;
mod repo_read_file;
mod repo_read_spans;
mod repo_search;

pub use bash::{BashInput, BashOutput, BashTool};
pub use clock::{ClockInput, ClockOutput, ClockTool};
pub use fs_write::{FsWriteInput, FsWriteOutput, FsWriteTool};
pub use repo_read_file::{RepoReadFileInput, RepoReadFileOutput, RepoReadFileTool};
pub use repo_read_spans::{RepoReadSpansInput, RepoReadSpansOutput, RepoReadSpansTool};
pub use repo_search::{RepoSearchInput, RepoSearchOutput, RepoSearchTool};
