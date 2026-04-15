//! Wire types connecting `TurnDriver` to the TUI approval surface.
//!
//! When `AuthorityEngine::authorize` returns `RequireApproval`, the driver
//! sends an `ApprovalRequestMsg` down the bridge and awaits a response on
//! the embedded oneshot. The TUI main loop surfaces the request as a modal
//! and converts keystrokes into `ApprovalResponse` values.

use crate::schemas::{ApprovalId, ApprovalScope, EffectClass, TurnId};
use tokio::sync::oneshot;

#[derive(Debug)]
pub struct ApprovalRequestMsg {
    pub turn_id: TurnId,
    pub approval_id: ApprovalId,
    pub tool_name: String,
    pub effect_class: EffectClass,
    pub summary: String,
    pub responder: oneshot::Sender<ApprovalResponse>,
}

#[derive(Debug)]
pub enum ApprovalResponse {
    Grant { scope: ApprovalScope },
    Deny,
}
