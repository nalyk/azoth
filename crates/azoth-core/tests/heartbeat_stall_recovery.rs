//! Chronon CP-2 — age-aware crash recovery.
//!
//! A dangling turn with at least one heartbeat is reclassified as
//! `TurnAborted { reason: Stalled }` carrying the last heartbeat `at`.
//! A dangling turn with no heartbeat stays the historical
//! `TurnInterrupted { reason: Crash, at: None }` — honest about the
//! absence of temporal evidence.

use azoth_core::event_store::{JsonlReader, JsonlWriter};
use azoth_core::schemas::{
    AbortReason, ContractId, HeartbeatProgress, RunId, SessionEvent, TurnId,
};
use tempfile::tempdir;

fn ts(s: &str) -> String {
    s.to_string()
}

#[test]
fn dangling_turn_with_heartbeat_recovers_as_stalled() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("run_stall.jsonl");
    let mut w = JsonlWriter::open(&path).unwrap();
    let run_id = RunId::from("run_stall".to_string());
    let turn_id = TurnId::from("t_stall".to_string());

    w.append(&SessionEvent::RunStarted {
        run_id: run_id.clone(),
        contract_id: ContractId::from("ctr".to_string()),
        timestamp: ts("2026-04-20T00:00:00Z"),
    })
    .unwrap();
    w.append(&SessionEvent::TurnStarted {
        turn_id: turn_id.clone(),
        run_id,
        parent_turn: None,
        timestamp: ts("2026-04-20T00:00:01Z"),
    })
    .unwrap();
    w.append(&SessionEvent::TurnHeartbeat {
        turn_id: turn_id.clone(),
        at: ts("2026-04-20T00:00:07Z"),
        progress: HeartbeatProgress {
            content_blocks: 3,
            tool_calls: 1,
            tokens_out: 42,
        },
    })
    .unwrap();
    // No terminal marker — dangling.
    drop(w);

    // Recover.
    let reader = JsonlReader::open(&path);
    let dangling = reader.recover_dangling_turns().unwrap();
    assert_eq!(dangling, vec![turn_id.clone()]);

    // Re-scan and assert a Stalled record was appended with the
    // heartbeat's `at`, not recovery-time's `at`.
    let reader = JsonlReader::open(&path);
    let forensic = reader.forensic().unwrap();
    let saw_stalled = forensic.iter().any(|fev| {
        matches!(
            &fev.event,
            SessionEvent::TurnAborted {
                reason: AbortReason::Stalled,
                at: Some(a),
                detail: Some(d),
                ..
            } if a == "2026-04-20T00:00:07Z" && d.contains("2026-04-20T00:00:07Z")
        )
    });
    assert!(
        saw_stalled,
        "recovery must emit Stalled carrying last heartbeat at"
    );
}

#[test]
fn dangling_turn_without_heartbeat_remains_crash() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("run_crash.jsonl");
    let mut w = JsonlWriter::open(&path).unwrap();
    let run_id = RunId::from("run_crash".to_string());
    let turn_id = TurnId::from("t_crash".to_string());

    w.append(&SessionEvent::RunStarted {
        run_id: run_id.clone(),
        contract_id: ContractId::from("ctr".to_string()),
        timestamp: ts("2026-04-20T00:00:00Z"),
    })
    .unwrap();
    w.append(&SessionEvent::TurnStarted {
        turn_id: turn_id.clone(),
        run_id,
        parent_turn: None,
        timestamp: ts("2026-04-20T00:00:01Z"),
    })
    .unwrap();
    // No heartbeat, no terminal marker.
    drop(w);

    let reader = JsonlReader::open(&path);
    let dangling = reader.recover_dangling_turns().unwrap();
    assert_eq!(dangling, vec![turn_id.clone()]);

    let reader = JsonlReader::open(&path);
    let forensic = reader.forensic().unwrap();
    let saw_crash = forensic.iter().any(|fev| {
        matches!(
            &fev.event,
            SessionEvent::TurnInterrupted {
                reason: AbortReason::Crash,
                at: None,
                ..
            }
        )
    });
    assert!(
        saw_crash,
        "no-heartbeat dangling turn must still recover as Crash (honest `at: None`)"
    );
}
