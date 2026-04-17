//! Append-only JSONL session event store with dual projection.
//!
//! Implements CRIT-1 from the architecture spec: the replayable projection
//! sees only fully-committed turns, so orphaned `tool_result` blocks are
//! structurally impossible in the model's replayed context.

pub mod jsonl;
pub mod migrations;
pub mod sqlite;

pub use jsonl::{
    ForensicEvent, JsonlReader, JsonlWriter, ProjectionError, ReplayableEvent, TurnOutcomeKind,
};
pub use sqlite::{MirrorError, SqliteMirror};
