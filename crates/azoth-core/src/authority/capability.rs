//! Capability tokens: deterministic grants the user mints through approval
//! modals. Held in AppState, persisted as events for replay/forensics.

use crate::schemas::{ApprovalScope, CapabilityTokenId, EffectClass};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct CapabilityToken {
    pub id: CapabilityTokenId,
    pub effect_class: EffectClass,
    pub tool_name: String,
    pub scope: ApprovalScope,
}

#[derive(Debug, Default)]
pub struct CapabilityStore {
    tokens: HashMap<CapabilityTokenId, CapabilityToken>,
}

impl CapabilityStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mint(&mut self, token: CapabilityToken) {
        self.tokens.insert(token.id.clone(), token);
    }

    pub fn revoke_once(&mut self, id: &CapabilityTokenId) -> Option<CapabilityToken> {
        self.tokens.remove(id)
    }

    /// Find a token that authorizes the given (tool, effect_class, optional path).
    pub fn find<'a>(
        &'a self,
        tool_name: &str,
        effect_class: EffectClass,
        path: Option<&str>,
    ) -> Option<&'a CapabilityToken> {
        self.tokens.values().find(|t| {
            t.tool_name == tool_name
                && t.effect_class == effect_class
                && match &t.scope {
                    ApprovalScope::Once => true,
                    ApprovalScope::Session => true,
                    ApprovalScope::ScopedPaths { paths } => match path {
                        Some(p) => paths.iter().any(|scoped| p.starts_with(scoped)),
                        None => false,
                    },
                }
        })
    }
}
