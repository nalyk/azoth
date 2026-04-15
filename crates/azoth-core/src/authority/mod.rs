//! Authority engine: taint tracking, secret handles, capability tokens,
//! approval policy. Deterministic controls outrank model output (invariant 2).

mod tainted;
mod secret;
mod engine;
mod capability;

pub use tainted::{Extractor, ExtractionError, JsonExtractor, Origin, Tainted};
pub use secret::SecretHandle;
pub use engine::{AuthorityDecision, AuthorityEngine, ApprovalPolicyV1};
pub use capability::{CapabilityToken, CapabilityStore};
