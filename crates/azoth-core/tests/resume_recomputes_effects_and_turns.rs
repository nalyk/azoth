//! Resume-time recompute: `JsonlReader::committed_run_progress` must rebuild
//! `(EffectCounter, turns_completed)` from the replayable projection so a
//! resuming worker seeds the contract's effect-budget and max-turn gates
//! exactly where the prior session left off. Effects inside non-committed
//! turns (aborted, interrupted) must be excluded — the live driver bumps the
//! counter only after the turn commits, and replay must match that
//! accounting.

use azoth_core::contract;
use azoth_core::event_store::{JsonlReader, JsonlWriter};
use azoth_core::schemas::{
    AbortReason, CommitOutcome, Contract, ContractId, EffectClass, EffectCounter, EffectRecord,
    EffectRecordId, RunId, SessionEvent, ToolUseId, TurnId, Usage,
};
use tempfile::tempdir;

fn ts() -> String {
    "2026-04-16T12:00:00Z".to_string()
}

fn accepted_contract() -> Contract {
    let mut c = contract::draft("seed resume recompute");
    c.success_criteria
        .push("cap enforced across resumes".into());
    c.effect_budget.max_apply_local = 5;
    c.effect_budget.max_apply_repo = 5;
    c
}

fn effect(class: EffectClass, name: &str) -> EffectRecord {
    EffectRecord {
        id: EffectRecordId::new(),
        tool_use_id: ToolUseId::new(),
        class,
        tool_name: name.to_string(),
        input_digest: None,
        output_artifact: None,
        error: None,
    }
}

#[test]
fn committed_run_progress_counts_only_committed_turn_effects() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut w = JsonlWriter::open(&path).unwrap();

    let run_id = RunId::from("run_resume".to_string());
    let contract_id = ContractId::from("ctr_resume".to_string());
    w.append(&SessionEvent::RunStarted {
        run_id: run_id.clone(),
        contract_id,
        timestamp: ts(),
    })
    .unwrap();
    w.append(&SessionEvent::ContractAccepted {
        contract: accepted_contract(),
        timestamp: ts(),
    })
    .unwrap();

    // Turn 1: committed, one apply_local effect.
    let t1 = TurnId::from("t_r1".to_string());
    w.append(&SessionEvent::TurnStarted {
        turn_id: t1.clone(),
        run_id: run_id.clone(),
        parent_turn: None,
        timestamp: ts(),
    })
    .unwrap();
    w.append(&SessionEvent::EffectRecord {
        turn_id: t1.clone(),
        effect: effect(EffectClass::ApplyLocal, "fs.write"),
    })
    .unwrap();
    w.append(&SessionEvent::TurnCommitted {
        turn_id: t1.clone(),
        outcome: CommitOutcome::Success,
        usage: Usage::default(),
    })
    .unwrap();

    // Turn 2: aborted, one apply_repo effect that must be EXCLUDED from the
    // tally (the live counter is only bumped for turns that go on to commit,
    // so replay must drop this effect).
    let t2 = TurnId::from("t_r2".to_string());
    w.append(&SessionEvent::TurnStarted {
        turn_id: t2.clone(),
        run_id: run_id.clone(),
        parent_turn: None,
        timestamp: ts(),
    })
    .unwrap();
    w.append(&SessionEvent::EffectRecord {
        turn_id: t2.clone(),
        effect: effect(EffectClass::ApplyRepo, "git.apply_patch"),
    })
    .unwrap();
    w.append(&SessionEvent::TurnAborted {
        turn_id: t2,
        reason: AbortReason::ValidatorFail,
        detail: Some("validator said no".into()),
        usage: Usage::default(),
    })
    .unwrap();

    // Turn 3: committed, one apply_local and one apply_repo effect.
    let t3 = TurnId::from("t_r3".to_string());
    w.append(&SessionEvent::TurnStarted {
        turn_id: t3.clone(),
        run_id: run_id.clone(),
        parent_turn: None,
        timestamp: ts(),
    })
    .unwrap();
    w.append(&SessionEvent::EffectRecord {
        turn_id: t3.clone(),
        effect: effect(EffectClass::ApplyLocal, "fs.write"),
    })
    .unwrap();
    w.append(&SessionEvent::EffectRecord {
        turn_id: t3.clone(),
        effect: effect(EffectClass::ApplyRepo, "git.apply_patch"),
    })
    .unwrap();
    // An Observe effect — must NOT move any counter (network_reads stays 0).
    w.append(&SessionEvent::EffectRecord {
        turn_id: t3.clone(),
        effect: effect(EffectClass::Observe, "repo.search"),
    })
    .unwrap();
    w.append(&SessionEvent::TurnCommitted {
        turn_id: t3,
        outcome: CommitOutcome::Success,
        usage: Usage::default(),
    })
    .unwrap();

    drop(w);

    let (effects, turns_completed) = JsonlReader::open(&path)
        .committed_run_progress()
        .expect("recompute succeeds");

    assert_eq!(
        effects,
        EffectCounter {
            apply_local: 2,
            apply_repo: 1,
            network_reads: 0
        },
        "aborted turn's apply_repo must be excluded; Observe must not bump"
    );
    assert_eq!(
        turns_completed, 2,
        "only committed turns count; aborted must not bump turns_completed"
    );
}

#[test]
fn committed_run_progress_on_fresh_session_is_zero() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut w = JsonlWriter::open(&path).unwrap();
    w.append(&SessionEvent::RunStarted {
        run_id: RunId::from("run_fresh".to_string()),
        contract_id: ContractId::from("ctr_fresh".to_string()),
        timestamp: ts(),
    })
    .unwrap();
    drop(w);

    let (effects, turns_completed) = JsonlReader::open(&path)
        .committed_run_progress()
        .expect("recompute succeeds on a no-turn session");
    assert_eq!(effects, EffectCounter::default());
    assert_eq!(turns_completed, 0);
}
