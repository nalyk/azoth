//! Structured telemetry events for the eval plane. v1 wraps `tracing`
//! with structured fields for every major lifecycle point. v2 adds
//! persistence and grader hooks.

use crate::schemas::{AbortReason, EffectClass, ValidatorStatus};
use tracing::{info, warn};

pub fn emit_turn_started(run_id: &str, turn_id: &str) {
    info!(run_id, turn_id, "turn_started");
}

pub fn emit_turn_committed(run_id: &str, turn_id: &str, input_tokens: u32, output_tokens: u32) {
    info!(run_id, turn_id, input_tokens, output_tokens, "turn_committed");
}

pub fn emit_turn_aborted(run_id: &str, turn_id: &str, reason: &str) {
    warn!(run_id, turn_id, reason, "turn_aborted");
}

pub fn emit_turn_interrupted(run_id: &str, turn_id: &str, reason: &str) {
    warn!(run_id, turn_id, reason, "turn_interrupted");
}

pub fn emit_model_request(run_id: &str, turn_id: &str, profile_id: &str) {
    info!(run_id, turn_id, profile_id, "model_request");
}

pub fn emit_tool_dispatch(
    run_id: &str,
    turn_id: &str,
    tool_name: &str,
    effect_class: EffectClass,
) {
    info!(
        run_id,
        turn_id,
        tool_name,
        effect_class = ?effect_class,
        "tool_dispatch",
    );
}

pub fn emit_tool_result(
    run_id: &str,
    turn_id: &str,
    tool_name: &str,
    is_error: bool,
    duration_ms: u64,
) {
    info!(
        run_id,
        turn_id,
        tool_name,
        is_error,
        duration_ms,
        "tool_result",
    );
}

pub fn emit_approval_requested(
    run_id: &str,
    turn_id: &str,
    tool_name: &str,
    effect_class: EffectClass,
) {
    info!(
        run_id,
        turn_id,
        tool_name,
        effect_class = ?effect_class,
        "approval_requested",
    );
}

pub fn emit_approval_granted(run_id: &str, turn_id: &str, tool_name: &str, scope: &str) {
    info!(run_id, turn_id, tool_name, scope, "approval_granted");
}

pub fn emit_approval_denied(run_id: &str, turn_id: &str, tool_name: &str) {
    warn!(run_id, turn_id, tool_name, "approval_denied");
}

pub fn emit_contract_accepted(run_id: &str, contract_id: &str) {
    info!(run_id, contract_id, "contract_accepted");
}

pub fn emit_validator_result(
    run_id: &str,
    turn_id: &str,
    validator: &str,
    status: ValidatorStatus,
) {
    info!(
        run_id,
        turn_id,
        validator,
        status = ?status,
        "validator_result",
    );
}

pub fn emit_context_compiled(
    run_id: &str,
    turn_id: &str,
    approximate_tokens: usize,
    evidence_count: usize,
) {
    info!(
        run_id,
        turn_id,
        approximate_tokens,
        evidence_count,
        "context_compiled",
    );
}

pub fn emit_session_resumed(run_id: &str, turns_recovered: u32, effects_recovered: u32) {
    info!(
        run_id,
        turns_recovered,
        effects_recovered,
        "session_resumed",
    );
}

pub fn emit_effect_budget_exhausted(
    run_id: &str,
    turn_id: &str,
    class: &str,
    used: u32,
    max: u32,
) {
    warn!(
        run_id,
        turn_id,
        class,
        used,
        max,
        "effect_budget_exhausted",
    );
}

pub fn emit_sandbox_prepared(run_id: &str, turn_id: &str, tier: &str) {
    info!(run_id, turn_id, tier, "sandbox_prepared");
}

pub fn emit_abort_reason(run_id: &str, turn_id: &str, reason: AbortReason, detail: &str) {
    warn!(
        run_id,
        turn_id,
        reason = ?reason,
        detail,
        "turn_abort_reason",
    );
}
