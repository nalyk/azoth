//! v1 built-in tools. Every tool is a concrete `Tool` impl with a typed
//! input struct.

mod repo_search;

pub use repo_search::{RepoSearchInput, RepoSearchOutput, RepoSearchTool};
