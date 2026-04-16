//! Context Kernel v0 — five-lane packet compiler.
//!
//! Packing rules (see draft_plan §"Context Kernel v0"):
//! 1. constitution first (stable prefix → cache key)
//! 2. critical evidence immediately after constitution
//!    (avoids Lost-in-the-Middle on long packets)
//! 3. exit criteria last
//! 4. long payloads stay as artifact refs, never inline
//! 5. transcript is never copied verbatim

mod evidence;
mod kernel;
mod tokenizer;

pub use evidence::{EvidenceCollector, LexicalEvidenceCollector};
pub use kernel::{ContextKernel, KernelError, StepInput};
pub use tokenizer::count_tokens;
// The tokenizer family flows into the kernel at packet-compile time; re-export
// it so callers do not need to reach into the adapter module directly.
pub use crate::adapter::TokenizerFamily;
