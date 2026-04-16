//! v1 authority engine. Hardcoded approval policy from draft_plan §
//! "Authority Engine — hardcoded v1 policy".

use super::{CapabilityStore, CapabilityToken};
use crate::schemas::{ApprovalId, ApprovalScope, CapabilityTokenId, EffectClass};

/// Decision yielded by `AuthorityEngine::authorize`.
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
    /// Effect class is not available in v1 — Tier C/D hook territory.
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
            eng.authorize("repo.search", EffectClass::Observe, None),
            AuthorityDecision::Auto
        );
    }

    #[test]
    fn apply_local_requires_approval_when_no_token() {
        let caps = CapabilityStore::new();
        let eng = AuthorityEngine::new(&caps, ApprovalPolicyV1);
        match eng.authorize("fs.write", EffectClass::ApplyLocal, Some("/tmp/x")) {
            AuthorityDecision::RequireApproval {
                tool_name,
                effect_class,
                ..
            } => {
                assert_eq!(tool_name, "fs.write");
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
        let tok = mint_from_approval("fs.write", EffectClass::ApplyLocal, ApprovalScope::Session);
        caps.mint(tok);
        let eng = AuthorityEngine::new(&caps, ApprovalPolicyV1);
        assert!(matches!(
            eng.authorize("fs.write", EffectClass::ApplyLocal, Some("/tmp/x")),
            AuthorityDecision::Reuse(_)
        ));
    }
}
