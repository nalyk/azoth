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

    /// Consume a capability iff its scope is `Once`. Returns the removed
    /// token on consumption; returns `None` for Session/ScopedPaths or an
    /// unknown id (call sites stay `if let` / `.is_some()` friendly).
    ///
    /// F0 (2026-04-25): `Once` is contracted with the user as a one-shot
    /// grant. Before this method was wired into `TurnDriver`, `find()` kept
    /// returning Once tokens on every subsequent authorize call — a live
    /// E2E run saw a single `approve once` for `fs_write` on
    /// `/tmp/smoke.txt` (which failed at the repo-root guard) silently
    /// cover two follow-up writes including a full rewrite of
    /// `Cargo.toml`. Call this from the `Reuse` arm so the next
    /// `apply_local`/`apply_repo` for the same tool re-prompts.
    pub fn consume_if_once(&mut self, id: &CapabilityTokenId) -> Option<CapabilityToken> {
        match self.tokens.get(id).map(|t| &t.scope) {
            Some(ApprovalScope::Once) => self.tokens.remove(id),
            _ => None,
        }
    }

    /// Expose the live token set for UX surfaces like `/approve` — the
    /// engine uses `find()` internally, the TUI needs something it can
    /// enumerate without reaching into private state (F1 2026-04-25).
    pub fn iter(&self) -> impl Iterator<Item = &CapabilityToken> {
        self.tokens.values()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mint_once(store: &mut CapabilityStore, tool: &str) -> CapabilityTokenId {
        let id = CapabilityTokenId::new();
        store.mint(CapabilityToken {
            id: id.clone(),
            effect_class: EffectClass::ApplyLocal,
            tool_name: tool.into(),
            scope: ApprovalScope::Once,
        });
        id
    }

    fn mint_session(store: &mut CapabilityStore, tool: &str) -> CapabilityTokenId {
        let id = CapabilityTokenId::new();
        store.mint(CapabilityToken {
            id: id.clone(),
            effect_class: EffectClass::ApplyLocal,
            tool_name: tool.into(),
            scope: ApprovalScope::Session,
        });
        id
    }

    #[test]
    fn consume_if_once_removes_once_tokens() {
        let mut store = CapabilityStore::new();
        let id = mint_once(&mut store, "fs_write");
        assert!(store
            .find("fs_write", EffectClass::ApplyLocal, None)
            .is_some());
        let consumed = store.consume_if_once(&id);
        assert!(consumed.is_some(), "Once token must be removed");
        assert!(
            store
                .find("fs_write", EffectClass::ApplyLocal, None)
                .is_none(),
            "subsequent find must not resurrect a consumed Once token"
        );
    }

    #[test]
    fn consume_if_once_preserves_session_tokens() {
        let mut store = CapabilityStore::new();
        let id = mint_session(&mut store, "fs_write");
        let consumed = store.consume_if_once(&id);
        assert!(
            consumed.is_none(),
            "Session scope must never be consumed by consume_if_once"
        );
        assert!(
            store
                .find("fs_write", EffectClass::ApplyLocal, None)
                .is_some(),
            "Session grant survives consume_if_once"
        );
    }

    #[test]
    fn consume_if_once_preserves_scoped_paths_tokens() {
        let mut store = CapabilityStore::new();
        let id = CapabilityTokenId::new();
        store.mint(CapabilityToken {
            id: id.clone(),
            effect_class: EffectClass::ApplyLocal,
            tool_name: "fs_write".into(),
            scope: ApprovalScope::ScopedPaths {
                paths: vec!["src/".into()],
            },
        });
        let consumed = store.consume_if_once(&id);
        assert!(
            consumed.is_none(),
            "ScopedPaths must never be consumed by consume_if_once"
        );
    }

    #[test]
    fn consume_if_once_on_missing_id_returns_none() {
        let mut store = CapabilityStore::new();
        let ghost = CapabilityTokenId::new();
        assert!(store.consume_if_once(&ghost).is_none());
    }

    #[test]
    fn iter_lists_all_minted_tokens() {
        let mut store = CapabilityStore::new();
        let _a = mint_once(&mut store, "fs_write");
        let _b = mint_session(&mut store, "bash");
        let names: Vec<&str> = store.iter().map(|t| t.tool_name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"fs_write"));
        assert!(names.contains(&"bash"));
    }
}
