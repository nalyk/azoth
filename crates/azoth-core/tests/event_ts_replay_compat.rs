//! Chronon CP-1 forward-compatibility — pre-CP-1 JSONL (no terminal `at`
//! field) must deserialize clean; post-CP-1 JSONL round-trips through the
//! event log preserving the timestamp.

use azoth_core::execution::{Clock, FrozenClock};
use azoth_core::schemas::{
    AbortReason, CommitOutcome, RunId, SessionEvent, TurnId, Usage, UsageDelta,
};

#[test]
fn pre_cp1_terminal_events_deserialize_without_at() {
    // Committed — no `at` field on the wire. Forward-compat required.
    let legacy = r#"{"type":"turn_committed","turn_id":"t_legacy","outcome":"success","usage":{"input_tokens":10,"output_tokens":5,"cache_read_tokens":0,"cache_write_tokens":0}}"#;
    let ev: SessionEvent = serde_json::from_str(legacy).expect("legacy committed deserializes");
    match ev {
        SessionEvent::TurnCommitted { at, .. } => assert!(at.is_none()),
        _ => panic!("expected TurnCommitted"),
    }

    let legacy_abort = r#"{"type":"turn_aborted","turn_id":"t_legacy","reason":"validator_fail","detail":"nope","usage":{"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0}}"#;
    let ev: SessionEvent = serde_json::from_str(legacy_abort).expect("legacy aborted deserializes");
    match ev {
        SessionEvent::TurnAborted { at, .. } => assert!(at.is_none()),
        _ => panic!("expected TurnAborted"),
    }

    let legacy_interrupt =
        r#"{"type":"turn_interrupted","turn_id":"t_legacy","reason":"user_cancel"}"#;
    let ev: SessionEvent =
        serde_json::from_str(legacy_interrupt).expect("legacy interrupted deserializes");
    match ev {
        SessionEvent::TurnInterrupted { at, .. } => assert!(at.is_none()),
        _ => panic!("expected TurnInterrupted"),
    }
}

#[test]
fn cp1_terminals_round_trip_timestamp() {
    let clock = FrozenClock::from_unix_secs(1_700_000_000);
    let ts = clock.now_iso();
    assert_eq!(ts, "2023-11-14T22:13:20Z");

    let ev = SessionEvent::TurnCommitted {
        turn_id: TurnId::from("t1".to_string()),
        outcome: CommitOutcome::Success,
        usage: Usage::default(),
        user_input: None,
        final_assistant: None,
        at: Some(ts.clone()),
    };
    let wire = serde_json::to_string(&ev).unwrap();
    assert!(wire.contains("\"at\":\"2023-11-14T22:13:20Z\""));

    let round: SessionEvent = serde_json::from_str(&wire).unwrap();
    match round {
        SessionEvent::TurnCommitted { at, .. } => assert_eq!(at.as_deref(), Some(ts.as_str())),
        _ => panic!("expected TurnCommitted"),
    }
}

#[test]
fn cp1_terminals_absent_at_is_omitted_on_wire() {
    let _ = RunId::from("r1".to_string()); // keep import
    let _ = UsageDelta::default();

    let ev = SessionEvent::TurnCommitted {
        turn_id: TurnId::from("t1".to_string()),
        outcome: CommitOutcome::Success,
        usage: Usage::default(),
        user_input: None,
        final_assistant: None,
        at: None,
    };
    let wire = serde_json::to_string(&ev).unwrap();
    // skip_serializing_if = "Option::is_none" → byte shape must not
    // contain the key at all. This is load-bearing for cache-prefix
    // stability: pre-CP-1 lines with no `at` and post-CP-1 lines that
    // happen to have None must be byte-identical.
    assert!(!wire.contains("\"at\""));

    let ev_abort = SessionEvent::TurnAborted {
        turn_id: TurnId::from("t1".to_string()),
        reason: AbortReason::ValidatorFail,
        detail: None,
        usage: Usage::default(),
        at: None,
    };
    let wire = serde_json::to_string(&ev_abort).unwrap();
    assert!(!wire.contains("\"at\""));
}
