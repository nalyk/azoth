//! Typed identifier newtypes.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

macro_rules! string_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new() -> Self {
                Self(format!("{}_{}", $prefix, short_uuid()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
    };
}

pub(crate) fn short_uuid() -> String {
    let id = Uuid::new_v4();
    let hex = id.simple().to_string();
    hex[..12].to_string()
}

string_id!(RunId, "run");
string_id!(TurnId, "t");
string_id!(ContractId, "ctr");
string_id!(CheckpointId, "chk");
string_id!(ContextPacketId, "ctx");
string_id!(ArtifactId, "art");
string_id!(CapabilityTokenId, "cap");
string_id!(ApprovalId, "apv");
string_id!(ToolUseId, "tu");
string_id!(EffectRecordId, "eff");

/// Groups parallel tool_use blocks so OpenAI adapter can restore their original
/// ordering on the downcast to `tool_calls`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CallGroupId(pub Uuid);

impl CallGroupId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for CallGroupId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for CallGroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cg_{}", self.0.simple())
    }
}
