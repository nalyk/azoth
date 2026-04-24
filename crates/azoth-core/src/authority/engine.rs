//! v1 authority engine. Hardcoded approval policy from draft_plan §
//! "Authority Engine — hardcoded v1 policy".

use super::{CapabilityStore, CapabilityToken};
use crate::schemas::{ApprovalId, ApprovalScope, CapabilityTokenId, EffectClass, EffectCounter};

/// β: ≤2 amend grants per open turn. Prevents a runaway "keep extending
/// until the model stops" pattern within a single turn.
pub const MAX_AMENDS_PER_TURN: u32 = 2;
/// β: ≤6 amend grants per run. Prevents the same runaway pattern from
/// being reset by turn boundaries.
pub const MAX_AMENDS_PER_RUN: u32 = 6;
/// β: proposed multiplier applied by the engine when offering a budget
/// extension. The actually-applied delta is clamped to ≤current by
/// `contract::apply_amend_clamped`, so the effective ceiling never more
/// than doubles per grant regardless of what the engine proposes.
pub const AMEND_PROPOSED_MULTIPLIER: u32 = 2;

/// Decision yielded by `AuthorityEngine::authorize` and
/// `AuthorityEngine::authorize_budget_extension`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorityDecision {
    /// Execution may proceed without bothering the user.
    Auto,
    /// Existing capability token covers this effect; reuse it.
    Reuse(CapabilityTokenId),
    /// A fresh approval is required from the user. The caller must open an
    /// ApprovalRequest, await the user's choice, and — on grant — mint a
    /// token with the returned approval_id.
    RequireApproval {
        approval_id: ApprovalId,
        tool_name: String,
        effect_class: EffectClass,
    },
    /// β: the contract's per-class effect budget has been exhausted and
    /// the brakes allow offering the user a one-shot extension.
    /// Distinct from `RequireApproval`: the grant here raises the
    /// ceiling and emits `SessionEvent::ContractAmended`; per-tool
    /// authorization still runs afterward.
    RequireBudgetExtension {
        approval_id: ApprovalId,
        label: &'static str,
        current: u32,
        /// Engine's proposed new ceiling, always `current *
        /// AMEND_PROPOSED_MULTIPLIER` in β. The UI shows this to the
        /// user; `contract::apply_amend_clamped` enforces the cap on
        /// commit.
        proposed: u32,
    },
    /// Effect class is not available in v1 — Tier C/D hook territory.
    /// Also used by `authorize_budget_extension` to surface brake-
    /// exceeded states (≤2/turn, ≤6/run) with a specific `hint`.
    NotAvailable { hint: &'static str },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ApprovalPolicyV1;

impl ApprovalPolicyV1 {
    pub const fn version(&self) -> &'static str {
        "policy_v1_hardcoded"
    }
}

pub struct AuthorityEngine<'a> {
    pub capabilities: &'a CapabilityStore,
    pub policy: ApprovalPolicyV1,
}

impl<'a> AuthorityEngine<'a> {
    pub fn new(capabilities: &'a CapabilityStore, policy: ApprovalPolicyV1) -> Self {
        Self {
            capabilities,
            policy,
        }
    }

    pub fn authorize(
        &self,
        tool_name: &str,
        effect_class: EffectClass,
        path_hint: Option<&str>,
    ) -> AuthorityDecision {
        if !effect_class.is_available_in_v1() {
            return AuthorityDecision::NotAvailable {
                hint: "scheduled for v2.5",
            };
        }

        match effect_class {
            EffectClass::Observe => AuthorityDecision::Auto,
            EffectClass::Stage => AuthorityDecision::Auto,
            EffectClass::ApplyLocal | EffectClass::ApplyRepo => {
                if let Some(token) = self.capabilities.find(tool_name, effect_class, path_hint) {
                    AuthorityDecision::Reuse(token.id.clone())
                } else {
                    AuthorityDecision::RequireApproval {
                        approval_id: ApprovalId::new(),
                        tool_name: tool_name.to_string(),
                        effect_class,
                    }
                }
            }
            // Tier C/D — already caught above but the exhaustive arm keeps
            // the compiler honest when a new variant lands.
            EffectClass::ApplyRemoteReversible
            | EffectClass::ApplyRemoteStateful
            | EffectClass::ApplyIrreversible => AuthorityDecision::NotAvailable {
                hint: "scheduled for v2.5",
            },
        }
    }

    /// β: check whether a budget-extension approval may be offered to
    /// the user. Returns `RequireBudgetExtension` when the brakes are
    /// clear; `NotAvailable { hint }` when either rate limit is hit.
    /// The driver wires this in at the budget-overflow branch instead
    /// of the bare abort.
    ///
    /// `counter` is read-only; incrementing `amends_this_turn` /
    /// `amends_this_run` happens only AFTER the user grants — at which
    /// point the driver also emits `ContractAmended` and bumps the
    /// ceiling bonus.
    pub fn authorize_budget_extension(
        &self,
        label: &'static str,
        current: u32,
        counter: &EffectCounter,
    ) -> AuthorityDecision {
        // Per-run brake checked first — hitting 6/run is the harder
        // signal to cross and takes precedence over the softer
        // per-turn limit for the error message.
        if counter.amends_this_run >= MAX_AMENDS_PER_RUN {
            return AuthorityDecision::NotAvailable {
                hint: "amend rate limit exceeded: max 6 per run",
            };
        }
        if counter.amends_this_turn >= MAX_AMENDS_PER_TURN {
            return AuthorityDecision::NotAvailable {
                hint: "amend rate limit exceeded: max 2 per turn",
            };
        }
        AuthorityDecision::RequireBudgetExtension {
            approval_id: ApprovalId::new(),
            label,
            current,
            proposed: current.saturating_mul(AMEND_PROPOSED_MULTIPLIER),
        }
    }
}

/// Helper for callers that have just received a user grant and need a token.
pub fn mint_from_approval(
    tool_name: &str,
    effect_class: EffectClass,
    scope: ApprovalScope,
) -> CapabilityToken {
    CapabilityToken {
        id: CapabilityTokenId::new(),
        effect_class,
        tool_name: tool_name.to_string(),
        scope,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_is_auto() {
        let caps = CapabilityStore::new();
        let eng = AuthorityEngine::new(&caps, ApprovalPolicyV1);
        assert_eq!(
            eng.authorize("repo_search", EffectClass::Observe, None),
            AuthorityDecision::Auto
        );
    }

    #[test]
    fn apply_local_requires_approval_when_no_token() {
        let caps = CapabilityStore::new();
        let eng = AuthorityEngine::new(&caps, ApprovalPolicyV1);
        match eng.authorize("fs_write", EffectClass::ApplyLocal, Some("/tmp/x")) {
            AuthorityDecision::RequireApproval {
                tool_name,
                effect_class,
                ..
            } => {
                assert_eq!(tool_name, "fs_write");
                assert_eq!(effect_class, EffectClass::ApplyLocal);
            }
            other => panic!("unexpected decision: {:?}", other),
        }
    }

    #[test]
    fn remote_stateful_not_available() {
        let caps = CapabilityStore::new();
        let eng = AuthorityEngine::new(&caps, ApprovalPolicyV1);
        assert!(matches!(
            eng.authorize("net.post", EffectClass::ApplyRemoteStateful, None),
            AuthorityDecision::NotAvailable { .. }
        ));
    }

    #[test]
    fn existing_session_token_is_reused() {
        let mut caps = CapabilityStore::new();
        let tok = mint_from_approval("fs_write", EffectClass::ApplyLocal, ApprovalScope::Session);
        caps.mint(tok);
        let eng = AuthorityEngine::new(&caps, ApprovalPolicyV1);
        assert!(matches!(
            eng.authorize("fs_write", EffectClass::ApplyLocal, Some("/tmp/x")),
            AuthorityDecision::Reuse(_)
        ));
    }
}
