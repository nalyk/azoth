//! Execution context threaded through every tool invocation.

use crate::artifacts::ArtifactStore;
use crate::schemas::{RunId, TurnId};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

#[derive(Debug, Clone)]
pub struct CancellationToken {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        }
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Resolves as soon as `cancel` has been or is called. Safe against the
    /// register-then-check race: we create the `Notified` future (which
    /// registers interest) *before* the final atomic check, so any cancel
    /// after that point is guaranteed to wake the waiter.
    pub async fn wait_cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
            if self.is_cancelled() {
                return;
            }
        }
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

pub struct ExecutionContextBuilder {
    run_id: RunId,
    turn_id: TurnId,
    artifacts: ArtifactStore,
    repo_root: std::path::PathBuf,
    cancellation: Option<CancellationToken>,
}

impl ExecutionContextBuilder {
    pub fn cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    pub fn build(self) -> ExecutionContext {
        ExecutionContext {
            run_id: self.run_id,
            turn_id: self.turn_id,
            artifacts: self.artifacts,
            cancellation: self.cancellation.unwrap_or_default(),
            repo_root: self.repo_root,
        }
    }
}

impl ExecutionContext {
    pub fn builder(
        run_id: RunId,
        turn_id: TurnId,
        artifacts: ArtifactStore,
        repo_root: std::path::PathBuf,
    ) -> ExecutionContextBuilder {
        ExecutionContextBuilder {
            run_id,
            turn_id,
            artifacts,
            repo_root,
            cancellation: None,
        }
    }

    pub fn cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}
