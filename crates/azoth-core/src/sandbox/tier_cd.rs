//! Tiers C (gVisor) and D (Firecracker) — architectural hooks only in v1.

use super::{Sandbox, SandboxError};
use crate::schemas::{EffectClass, SandboxTier};

pub struct TierC;
pub struct TierD;

impl Sandbox for TierC {
    fn tier(&self) -> SandboxTier {
        SandboxTier::C
    }
    fn prepare(&self) -> Result<(), SandboxError> {
        Err(SandboxError::EffectNotAvailable(
            EffectClass::ApplyRemoteReversible,
        ))
    }
}

impl Sandbox for TierD {
    fn tier(&self) -> SandboxTier {
        SandboxTier::D
    }
    fn prepare(&self) -> Result<(), SandboxError> {
        Err(SandboxError::EffectNotAvailable(
            EffectClass::ApplyIrreversible,
        ))
    }
}
