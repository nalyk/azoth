//! Authority engine: taint tracking, secret handles, capability tokens,
//! approval policy. Deterministic controls outrank model output (invariant 2).

pub mod approval_bridge;
mod capability;
mod engine;
mod secret;
mod tainted;

pub use approval_bridge::{ApprovalRequestMsg, ApprovalResponse};
pub use capability::{CapabilityStore, CapabilityToken};
pub use engine::{mint_from_approval, ApprovalPolicyV1, AuthorityDecision, AuthorityEngine};
pub use secret::SecretHandle;
pub use tainted::{ExtractionError, Extractor, JsonExtractor, Origin, Tainted};
