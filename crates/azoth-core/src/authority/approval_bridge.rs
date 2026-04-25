//! Wire types connecting `TurnDriver` to the TUI approval surface.
//!
//! When `AuthorityEngine::authorize` returns `RequireApproval`, the driver
//! sends an `ApprovalRequestMsg` down the bridge and awaits a response on
//! the embedded oneshot. The TUI main loop surfaces the request as a modal
//! and converts keystrokes into `ApprovalResponse` values.

use crate::schemas::{ApprovalId, ApprovalScope, EffectClass, TurnId};
use tokio::sync::oneshot;

/// β: per-message kind discriminator. `PerTool` is the v1 shape; the
/// `BudgetExtension` variant carries the extra data the TUI needs to
/// render a budget-extension approval distinctly from a per-tool one.
///
/// An optional field rather than an enum split keeps the single-field-
/// addition migration cheap: existing construct sites set `budget_extension:
/// None` and existing consumers that ignore it keep their behaviour. Only
/// TUI renderers opt in.
#[derive(Debug, Clone)]
pub struct BudgetExtensionRequest {
    /// Budget class label — in β, only `"apply_local"` or
    /// `"apply_repo"` flow through the driver's amend path. The
    /// underlying `EffectBudget` struct also carries
    /// `max_network_reads`, and `EffectBudgetDelta` mirrors that
    /// three-field shape for cache-prefix / schema consistency, but
    /// no v1-v2 `EffectClass` variant maps to network reads yet;
    /// `network_reads` is scaffolding, not a live amend target.
    ///
    /// When a `NetworkReads` `EffectClass` lands in a future sprint,
    /// the driver's budget-overflow branch picks it up by adding
    /// one more arm to the class-to-(used, max, bonus, label) match
    /// in `turn/mod.rs`; no schema change needed here.
    ///
    /// `&'static str` because these names are compile-time constants
    /// on `EffectBudget`; avoids allocation and lets log formatters
    /// pass the pointer directly.
    pub label: &'static str,
    /// The effective ceiling at the moment of the overflow — base
    /// contract value plus prior amends already in play.
    ///
    /// Cannot be `0`: `AuthorityEngine::authorize_budget_extension`
    /// returns `NotAvailable` rather than `RequireBudgetExtension`
    /// when `current == 0`, because `2 × 0 = 0` would mean any grant
    /// is a no-op bypass. The zero case is locked by test
    /// `zero_current_is_not_available_even_when_brakes_clear`.
    pub current: u32,
    /// The engine's proposed new ceiling after this amend. Always
    /// `current × 2` in β; future variants may propose a different
    /// multiple and rely on the ≤2× clamp in `contract::apply_amend_clamped`.
    pub proposed: u32,
}

#[derive(Debug)]
pub struct ApprovalRequestMsg {
    pub turn_id: TurnId,
    pub approval_id: ApprovalId,
    pub tool_name: String,
    pub effect_class: EffectClass,
    pub summary: String,
    pub responder: oneshot::Sender<ApprovalResponse>,
    /// β: `Some` when the driver requests a mid-run budget extension
    /// (not a per-tool approval). TUI surfaces render this distinctly.
    /// `None` = legacy per-tool flow.
    pub budget_extension: Option<BudgetExtensionRequest>,
    /// F4 (2026-04-25): driver-side preflight warning the sheet
    /// should render before the user taps approve. Currently set
    /// when the authorize path_hint canonicalizes outside
    /// `ctx.repo_root` for fs_write / fs_delete — the tool's own
    /// guard will reject the call even on approval, so the user is
    /// about to waste a grant. `None` = no warning, render the
    /// sheet normally.
    pub path_warning: Option<String>,
}

#[derive(Debug)]
pub enum ApprovalResponse {
    Grant { scope: ApprovalScope },
    Deny,
}
