//! Execution context threaded through every tool invocation.

use crate::artifacts::ArtifactStore;
use crate::schemas::{RunId, TurnId};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    flag: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

pub struct ExecutionContext {
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub artifacts: ArtifactStore,
    pub cancellation: CancellationToken,
    /// Absolute path to the repo the tool may read. Tools above `Observe`
    /// get a fuse-overlayfs merged view rooted at this path; for now the
    /// field is informational.
    pub repo_root: std::path::PathBuf,
}

impl ExecutionContext {
    pub fn cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}
