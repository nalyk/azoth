//! Chronon CP-5 (forensic as-of): `JsonlReader::forensic_as_of(t)` and
//! its companion bounded APIs.
//!
//! Locks in five behaviors:
//!
//! 1. A turn whose `TurnCommitted.at` is after `as_of` is dropped whole
//!    (including its ContentBlocks / ToolResults) — atomicity extends
//!    to the time axis.
//! 2. A turn whose terminal `at` is ≤ `as_of` is fully included.
//! 3. Pre-CP-1 terminals (terminal `at == None`) fall back to
//!    `TurnStarted.timestamp` for visibility.
//! 4. `last_accepted_contract_as_of` returns the pre-`as_of` winner.
//! 5. `committed_run_progress_as_of` counts only committed turns with
//!    effective `at` ≤ `as_of` — aborted turns under the cutoff still
//!    drop out, matching `committed_run_progress`.

use azoth_core::contract;
use azoth_core::event_store::jsonl::{JsonlReader, JsonlWriter};
use azoth_core::schemas::{
    AbortReason, CommitOutcome, ContentBlock, ContractId, EffectClass, EffectRecord,
    EffectRecordId, RunId, SessionEvent, ToolUseId, TurnId, Usage,
};
use tempfile::tempdir;

fn run_id() -> RunId {
    RunId::from("run_asof".to_string())
}

fn contract_id() -> ContractId {
    ContractId::from("ctr_asof".to_string())
}

fn ts(s: &str) -> String {
    s.to_string()
}

fn write_start(w: &mut JsonlWriter, turn_id: &TurnId, started: &str) {
    w.append(&SessionEvent::TurnStarted {
        turn_id: turn_id.clone(),
        run_id: run_id(),
        parent_turn: None,
        timestamp: ts(started),
    })
    .unwrap();
}

fn write_commit(
    w: &mut JsonlWriter,
    turn_id: &TurnId,
    at: Option<&str>,
    user: &str,
    assistant: &str,
) {
    w.append(&SessionEvent::TurnCommitted {
        turn_id: turn_id.clone(),
        outcome: CommitOutcome::Success,
        usage: Usage::default(),
        user_input: Some(vec![ContentBlock::Text {
            text: user.to_string(),
        }]),
        final_assistant: Some(vec![ContentBlock::Text {
            text: assistant.to_string(),
        }]),
        at: at.map(ToString::to_string),
    })
    .unwrap();
}

fn write_run_started(w: &mut JsonlWriter, timestamp: &str) {
    w.append(&SessionEvent::RunStarted {
        run_id: run_id(),
        contract_id: contract_id(),
        timestamp: ts(timestamp),
    })
    .unwrap();
}

fn apply_local_effect() -> SessionEvent {
    SessionEvent::EffectRecord {
        turn_id: TurnId::from("will_be_overwritten".to_string()),
        effect: EffectRecord {
            id: EffectRecordId::new(),
            tool_use_id: ToolUseId::from("tu_asof".to_string()),
            class: EffectClass::ApplyLocal,
            tool_name: "fs_write".to_string(),
            input_digest: None,
            output_artifact: None,
            error: None,
        },
    }
}

#[test]
fn forensic_as_of_drops_turns_committed_after_cutoff() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut w = JsonlWriter::open(&path).unwrap();

    write_run_started(&mut w, "2026-04-20T10:00:00Z");

    let t1 = TurnId::from("t_before".to_string());
    let t2 = TurnId::from("t_after".to_string());

    write_start(&mut w, &t1, "2026-04-20T10:01:00Z");
    w.append(&SessionEvent::ContentBlock {
        turn_id: t1.clone(),
        index: 0,
        block: ContentBlock::Text {
            text: "early work".into(),
        },
    })
    .unwrap();
    write_commit(&mut w, &t1, Some("2026-04-20T10:02:00Z"), "hi", "hello");

    write_start(&mut w, &t2, "2026-04-20T10:05:00Z");
    w.append(&SessionEvent::ContentBlock {
        turn_id: t2.clone(),
        index: 0,
        block: ContentBlock::Text {
            text: "later work".into(),
        },
    })
    .unwrap();
    write_commit(&mut w, &t2, Some("2026-04-20T10:06:00Z"), "more", "ok");

    drop(w);

    let r = JsonlReader::open(&path);
    let cutoff = "2026-04-20T10:03:00Z";
    let forensic = r.forensic_as_of(cutoff).unwrap();

    // Turn 1 events should survive; Turn 2 events should be dropped whole.
    let t1_count = forensic
        .iter()
        .filter(|f| f.event.turn_id() == Some(&t1))
        .count();
    let t2_count = forensic
        .iter()
        .filter(|f| f.event.turn_id() == Some(&t2))
        .count();
    assert!(t1_count > 0, "t_before must survive");
    assert_eq!(t2_count, 0, "t_after must be dropped whole");

    // RunStarted survives because its timestamp is before cutoff.
    let has_run_started = forensic
        .iter()
        .any(|f| matches!(&f.event, SessionEvent::RunStarted { .. }));
    assert!(has_run_started);
}

#[test]
fn forensic_as_of_falls_back_to_turn_started_when_terminal_at_is_none() {
    // Pre-CP-1 committed terminal (at: None) with TurnStarted.timestamp
    // BEFORE the cutoff → still visible.
    let dir = tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut w = JsonlWriter::open(&path).unwrap();

    write_run_started(&mut w, "2026-04-20T09:00:00Z");
    let legacy = TurnId::from("t_legacy".to_string());
    write_start(&mut w, &legacy, "2026-04-20T09:30:00Z");
    write_commit(&mut w, &legacy, None, "legacy-in", "legacy-out");

    let modern_before = TurnId::from("t_modern_in".to_string());
    write_start(&mut w, &modern_before, "2026-04-20T10:00:00Z");
    write_commit(
        &mut w,
        &modern_before,
        Some("2026-04-20T10:00:10Z"),
        "mi",
        "mo",
    );

    let modern_after = TurnId::from("t_modern_out".to_string());
    write_start(&mut w, &modern_after, "2026-04-20T11:30:00Z");
    write_commit(
        &mut w,
        &modern_after,
        Some("2026-04-20T11:30:05Z"),
        "xi",
        "xo",
    );
    drop(w);

    let r = JsonlReader::open(&path);
    let forensic = r.forensic_as_of("2026-04-20T11:00:00Z").unwrap();

    let visible: std::collections::HashSet<_> = forensic
        .iter()
        .filter_map(|f| f.event.turn_id().cloned())
        .collect();
    assert!(
        visible.contains(&legacy),
        "legacy turn visible via TurnStarted fallback"
    );
    assert!(visible.contains(&modern_before));
    assert!(!visible.contains(&modern_after));
}

#[test]
fn last_accepted_contract_as_of_returns_prior_winner() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut w = JsonlWriter::open(&path).unwrap();

    write_run_started(&mut w, "2026-04-20T10:00:00Z");

    let mut early = contract::draft("early goal");
    early.id = ContractId::from("ctr_early".to_string());
    w.append(&SessionEvent::ContractAccepted {
        contract: early.clone(),
        timestamp: ts("2026-04-20T10:01:00Z"),
    })
    .unwrap();

    let mut late = contract::draft("late goal");
    late.id = ContractId::from("ctr_late".to_string());
    w.append(&SessionEvent::ContractAccepted {
        contract: late.clone(),
        timestamp: ts("2026-04-20T10:10:00Z"),
    })
    .unwrap();

    drop(w);

    let r = JsonlReader::open(&path);
    // Before both: None.
    assert!(r
        .last_accepted_contract_as_of("2026-04-20T09:00:00Z")
        .unwrap()
        .is_none());
    // Between: early wins.
    let mid = r
        .last_accepted_contract_as_of("2026-04-20T10:05:00Z")
        .unwrap()
        .expect("early contract should be visible");
    assert_eq!(mid.id.as_str(), "ctr_early");
    // After both: late wins.
    let final_ = r
        .last_accepted_contract_as_of("2026-04-20T11:00:00Z")
        .unwrap()
        .expect("late contract should be visible");
    assert_eq!(final_.id.as_str(), "ctr_late");
}

#[test]
fn committed_run_progress_as_of_counts_only_pre_cutoff_committed_turns() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut w = JsonlWriter::open(&path).unwrap();

    write_run_started(&mut w, "2026-04-20T10:00:00Z");

    // Pre-cutoff committed turn with one ApplyLocal effect.
    let t_pre = TurnId::from("t_pre".to_string());
    write_start(&mut w, &t_pre, "2026-04-20T10:01:00Z");
    let mut pre_effect = apply_local_effect();
    if let SessionEvent::EffectRecord { turn_id, .. } = &mut pre_effect {
        *turn_id = t_pre.clone();
    }
    w.append(&pre_effect).unwrap();
    write_commit(
        &mut w,
        &t_pre,
        Some("2026-04-20T10:02:00Z"),
        "p-in",
        "p-out",
    );

    // Pre-cutoff aborted turn — effect should NOT count (matches the
    // committed-only semantics of `committed_run_progress`).
    let t_abort = TurnId::from("t_abort".to_string());
    write_start(&mut w, &t_abort, "2026-04-20T10:03:00Z");
    let mut abort_effect = apply_local_effect();
    if let SessionEvent::EffectRecord { turn_id, .. } = &mut abort_effect {
        *turn_id = t_abort.clone();
    }
    w.append(&abort_effect).unwrap();
    w.append(&SessionEvent::TurnAborted {
        turn_id: t_abort.clone(),
        reason: AbortReason::ValidatorFail,
        detail: None,
        usage: Usage::default(),
        at: Some(ts("2026-04-20T10:04:00Z")),
    })
    .unwrap();

    // Post-cutoff committed turn — must NOT count.
    let t_post = TurnId::from("t_post".to_string());
    write_start(&mut w, &t_post, "2026-04-20T10:10:00Z");
    let mut post_effect = apply_local_effect();
    if let SessionEvent::EffectRecord { turn_id, .. } = &mut post_effect {
        *turn_id = t_post.clone();
    }
    w.append(&post_effect).unwrap();
    write_commit(
        &mut w,
        &t_post,
        Some("2026-04-20T10:11:00Z"),
        "px-in",
        "px-out",
    );

    drop(w);

    let r = JsonlReader::open(&path);
    let (effects, completed) = r
        .committed_run_progress_as_of("2026-04-20T10:05:00Z")
        .unwrap();
    assert_eq!(completed, 1, "only t_pre committed before cutoff");
    assert_eq!(effects.apply_local, 1, "only t_pre's ApplyLocal counts");
}

#[test]
fn rebuild_history_as_of_returns_bounded_committed_exchanges() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut w = JsonlWriter::open(&path).unwrap();

    write_run_started(&mut w, "2026-04-20T10:00:00Z");

    let t1 = TurnId::from("t_one".to_string());
    write_start(&mut w, &t1, "2026-04-20T10:01:00Z");
    write_commit(&mut w, &t1, Some("2026-04-20T10:02:00Z"), "a", "A");

    let t2 = TurnId::from("t_two".to_string());
    write_start(&mut w, &t2, "2026-04-20T10:10:00Z");
    write_commit(&mut w, &t2, Some("2026-04-20T10:11:00Z"), "b", "B");

    drop(w);

    let r = JsonlReader::open(&path);
    let history = r.rebuild_history_as_of("2026-04-20T10:05:00Z").unwrap();
    // One turn ⇒ one user + one assistant message.
    assert_eq!(history.len(), 2);
    match &history[0].content.first().unwrap() {
        ContentBlock::Text { text } => assert_eq!(text, "a"),
        other => panic!("unexpected {other:?}"),
    }
}

/// Regression — the scan was comparing RFC3339 timestamps
/// lexicographically. Fractional seconds broke that: `.` (0x2E) < `Z`
/// (0x5A) in ASCII, so an event at `2026-04-20T10:02:00.500Z` sorts
/// *before* a cutoff at `2026-04-20T10:02:00Z` as strings, even though
/// it is chronologically *after*. The fix parses both sides as
/// `OffsetDateTime` before comparing.
#[test]
fn as_of_handles_fractional_second_events() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut w = JsonlWriter::open(&path).unwrap();

    write_run_started(&mut w, "2026-04-20T10:00:00Z");

    // Turn at EXACTLY the cutoff (fractional-seconds zero) — included.
    let t_before = TurnId::from("t_before".to_string());
    write_start(&mut w, &t_before, "2026-04-20T10:01:00Z");
    write_commit(&mut w, &t_before, Some("2026-04-20T10:02:00Z"), "a", "A");

    // Turn 500ms AFTER the cutoff — must be excluded.
    // Lexicographic compare would have included this (`.` < `Z`).
    let t_after = TurnId::from("t_after".to_string());
    write_start(&mut w, &t_after, "2026-04-20T10:02:00.100Z");
    write_commit(&mut w, &t_after, Some("2026-04-20T10:02:00.500Z"), "b", "B");

    drop(w);

    let r = JsonlReader::open(&path);
    let cutoff = "2026-04-20T10:02:00Z";
    let forensic = r.forensic_as_of(cutoff).unwrap();

    let committed_ids: Vec<String> = forensic
        .iter()
        .filter_map(|f| match &f.event {
            SessionEvent::TurnCommitted { turn_id, .. } => Some(turn_id.as_str().to_string()),
            _ => None,
        })
        .collect();

    assert_eq!(
        committed_ids,
        vec!["t_before".to_string()],
        "only the at-or-before-cutoff turn should be visible (got {committed_ids:?})"
    );
}

#[test]
fn as_of_malformed_input_surfaces_error() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut w = JsonlWriter::open(&path).unwrap();
    write_run_started(&mut w, "2026-04-20T10:00:00Z");
    drop(w);

    let r = JsonlReader::open(&path);
    let err = r.forensic_as_of("not-a-timestamp").unwrap_err();
    match err {
        azoth_core::event_store::jsonl::ProjectionError::MalformedAsOf { input, .. } => {
            assert_eq!(input, "not-a-timestamp");
        }
        other => panic!("expected MalformedAsOf, got {other:?}"),
    }
}
