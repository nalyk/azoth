//! Structured telemetry events for the eval plane. v1 is a thin wrapper
//! around `tracing`; v2 adds persistence and grader hooks.

use tracing::info;

pub fn emit_turn_started(run_id: &str, turn_id: &str) {
    info!(run_id, turn_id, "turn_started");
}

pub fn emit_turn_committed(run_id: &str, turn_id: &str, total_tokens: u32) {
    info!(run_id, turn_id, total_tokens, "turn_committed");
}

pub fn emit_turn_aborted(run_id: &str, turn_id: &str, reason: &str) {
    info!(run_id, turn_id, reason, "turn_aborted");
}
