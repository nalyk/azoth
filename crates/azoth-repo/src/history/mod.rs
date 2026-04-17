//! Sprint 3 — repo *history* plane.
//!
//! Builds a co-edit graph from recent `git log` commits and exposes it
//! through `azoth_core::retrieval::GraphRetrieval` so the context
//! kernel can surface "files that change together" as evidence.
//!
//! Layout:
//! - `git_cli` — thin `Command::new("git")` wrapper. Single source of
//!   stdin/stderr discipline; every other module calls through here
//!   so the subprocess boundary is one place to audit.
//! - `co_edit` — the graph builder. Takes the commit stream from
//!   `git_cli` and writes `co_edit_edges` rows.
//! - `graph_retrieval` — `CoEditGraphRetrieval`, the read side that
//!   answers `GraphRetrieval::neighbors`.

pub mod co_edit;
pub mod git_cli;
pub mod graph_retrieval;

pub use co_edit::{build, CoEditBuildStats, CoEditError};
pub use git_cli::{recent_commits, Commit, GitError};
pub use graph_retrieval::{path_node, CoEditGraphRetrieval, PATH_PREFIX};
