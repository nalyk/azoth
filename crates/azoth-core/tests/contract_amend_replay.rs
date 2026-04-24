//! β: seed a session with ContractAccepted(budget=20) + 10 committed
//! `apply_local` EffectRecords + ContractAmended(+20). Assert
//! `committed_run_progress` returns `apply_local = 10` AND the
//! `apply_local_ceiling_bonus = 20`, and `last_effective_contract`
//! returns `max_apply_local = 40`.
//!
//! Covers the invariant-6 property "every subsystem is eval-able":
//! resume must converge to exactly the effective ceiling the live
//! driver saw at the last committed turn.

use azoth_core::event_store::{JsonlReader, JsonlWriter};
use azoth_core::schemas::{
    CommitOutcome, Contract, ContractId, EffectBudget, EffectBudgetDelta, EffectClass,
    EffectRecord, EffectRecordId, RunId, Scope, SessionEvent, ToolUseId, TurnId, Usage,
};

fn contract() -> Contract {
    Contract {
        id: ContractId::from("ctr_replay".to_string()),
        goal: "replay accounting".into(),
        non_goals: Vec::new(),
        success_criteria: vec!["numbers agree".into()],
        scope: Scope {
            include_paths: vec![".".into()],
            exclude_paths: Vec::new(),
            max_turns: Some(32),
            max_wall_secs: None,
        },
        effect_budget: EffectBudget {
            max_apply_local: 20,
            max_apply_repo: 5,
            max_network_reads: 0,
        },
        notes: vec!["accepted".into()],
    }
}

#[test]
fn replay_folds_ten_effects_plus_one_amend_into_expected_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut w = JsonlWriter::open(&path).unwrap();

    let run_id = RunId::from("run_replay".to_string());
    let turn_id = TurnId::from("t_replay".to_string());
    let c = contract();

    w.append(&SessionEvent::RunStarted {
        run_id: run_id.clone(),
        contract_id: c.id.clone(),
        timestamp: "2026-04-24T19:00:00Z".into(),
    })
    .unwrap();
    w.append(&SessionEvent::ContractAccepted {
        contract: c.clone(),
        timestamp: "2026-04-24T19:00:01Z".into(),
    })
    .unwrap();
    w.append(&SessionEvent::TurnStarted {
        turn_id: turn_id.clone(),
        run_id: run_id.clone(),
        parent_turn: None,
        timestamp: "2026-04-24T19:00:02Z".into(),
    })
    .unwrap();
    for i in 0..10 {
        w.append(&SessionEvent::EffectRecord {
            turn_id: turn_id.clone(),
            effect: EffectRecord {
                id: EffectRecordId::from(format!("er_{i}")),
                tool_use_id: ToolUseId::from(format!("tu_{i}")),
                class: EffectClass::ApplyLocal,
                tool_name: "fs_write".into(),
                input_digest: None,
                output_artifact: None,
                error: None,
            },
        })
        .unwrap();
    }
    w.append(&SessionEvent::ContractAmended {
        contract_id: c.id.clone(),
        turn_id: turn_id.clone(),
        delta: EffectBudgetDelta {
            apply_local: 20,
            apply_repo: 0,
            network_reads: 0,
        },
        at: "2026-04-24T19:00:03Z".into(),
    })
    .unwrap();
    w.append(&SessionEvent::TurnCommitted {
        turn_id: turn_id.clone(),
        outcome: CommitOutcome::Success,
        usage: Usage::default(),
        user_input: None,
        final_assistant: None,
        at: Some("2026-04-24T19:00:04Z".into()),
    })
    .unwrap();
    drop(w);

    let r = JsonlReader::open(&path);
    let (effects, turns_completed) = r.committed_run_progress().unwrap();

    assert_eq!(effects.apply_local, 10, "10 EffectRecords folded");
    assert_eq!(
        effects.apply_local_ceiling_bonus, 20,
        "amend delta folded into bonus"
    );
    assert_eq!(effects.amends_this_run, 1, "one amend in this run");
    assert_eq!(
        effects.amends_this_turn, 0,
        "amends_this_turn stays at 0 on resume — per-turn state resets on drive_turn entry"
    );
    assert_eq!(turns_completed, 1);

    let effective = r.last_effective_contract().unwrap().expect("contract");
    assert_eq!(
        effective.effect_budget.max_apply_local, 40,
        "base 20 + amend delta 20 = 40"
    );
    assert_eq!(effective.effect_budget.max_apply_repo, 5, "untouched");
}

#[test]
fn fold_progress_ignores_stale_amends_across_contract_replacement() {
    // R1 (PR #31 gemini HIGH + codex P2): if a mid-session
    // ContractAccepted supersedes the prior contract, a
    // ContractAmended event that targeted the OLD contract_id must
    // not inflate the ceiling_bonus counter returned by
    // committed_run_progress. Otherwise a resuming driver would
    // overstate the effective ceiling under the new contract and
    // the gate-check math diverges from the live driver's.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut w = JsonlWriter::open(&path).unwrap();

    let run_id = RunId::from("run_stale".to_string());
    let turn_old = TurnId::from("t_old".to_string());
    let turn_new = TurnId::from("t_new".to_string());
    let old = Contract {
        id: ContractId::from("ctr_stale_old".to_string()),
        ..contract()
    };
    let new = Contract {
        id: ContractId::from("ctr_stale_new".to_string()),
        ..contract()
    };

    w.append(&SessionEvent::RunStarted {
        run_id: run_id.clone(),
        contract_id: old.id.clone(),
        timestamp: "2026-04-24T21:00:00Z".into(),
    })
    .unwrap();
    w.append(&SessionEvent::ContractAccepted {
        contract: old.clone(),
        timestamp: "2026-04-24T21:00:01Z".into(),
    })
    .unwrap();
    w.append(&SessionEvent::TurnStarted {
        turn_id: turn_old.clone(),
        run_id: run_id.clone(),
        parent_turn: None,
        timestamp: "2026-04-24T21:00:02Z".into(),
    })
    .unwrap();
    // Amend against OLD contract.
    w.append(&SessionEvent::ContractAmended {
        contract_id: old.id.clone(),
        turn_id: turn_old.clone(),
        delta: EffectBudgetDelta {
            apply_local: 50,
            apply_repo: 0,
            network_reads: 0,
        },
        at: "2026-04-24T21:00:03Z".into(),
    })
    .unwrap();
    w.append(&SessionEvent::TurnCommitted {
        turn_id: turn_old.clone(),
        outcome: CommitOutcome::Success,
        usage: Usage::default(),
        user_input: None,
        final_assistant: None,
        at: Some("2026-04-24T21:00:04Z".into()),
    })
    .unwrap();
    // NEW contract accepted → old amend becomes stale.
    w.append(&SessionEvent::ContractAccepted {
        contract: new.clone(),
        timestamp: "2026-04-24T21:00:05Z".into(),
    })
    .unwrap();
    // One real amend against the new contract.
    w.append(&SessionEvent::TurnStarted {
        turn_id: turn_new.clone(),
        run_id: run_id.clone(),
        parent_turn: None,
        timestamp: "2026-04-24T21:00:06Z".into(),
    })
    .unwrap();
    w.append(&SessionEvent::ContractAmended {
        contract_id: new.id.clone(),
        turn_id: turn_new.clone(),
        delta: EffectBudgetDelta {
            apply_local: 7,
            apply_repo: 0,
            network_reads: 0,
        },
        at: "2026-04-24T21:00:07Z".into(),
    })
    .unwrap();
    w.append(&SessionEvent::TurnCommitted {
        turn_id: turn_new.clone(),
        outcome: CommitOutcome::Success,
        usage: Usage::default(),
        user_input: None,
        final_assistant: None,
        at: Some("2026-04-24T21:00:08Z".into()),
    })
    .unwrap();
    drop(w);

    let r = JsonlReader::open(&path);
    let (effects, _) = r.committed_run_progress().unwrap();
    assert_eq!(
        effects.apply_local_ceiling_bonus, 7,
        "only the new-contract amend delta (7) must count; \
         the stale amend (50) against the superseded contract_id must be dropped"
    );
    assert_eq!(
        effects.amends_this_run, 1,
        "only one amend targets the current contract"
    );

    let effective = r.last_effective_contract().unwrap().expect("contract");
    assert_eq!(effective.id, new.id);
    assert_eq!(
        effective.effect_budget.max_apply_local,
        20 + 7,
        "effective ceiling reflects only the new-contract amend"
    );
}

#[test]
fn replay_ignores_amend_for_different_contract_id() {
    // Scenario: a prior contract was amended, then a fresh contract is
    // accepted mid-session. The fresh contract MUST start with its own
    // budget — amends from the old contract id must not leak into it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut w = JsonlWriter::open(&path).unwrap();

    let run_id = RunId::from("run_iso".to_string());
    let turn_id = TurnId::from("t_iso".to_string());
    let old = Contract {
        id: ContractId::from("ctr_old".to_string()),
        ..contract()
    };
    let new = Contract {
        id: ContractId::from("ctr_new".to_string()),
        ..contract()
    };

    w.append(&SessionEvent::RunStarted {
        run_id: run_id.clone(),
        contract_id: old.id.clone(),
        timestamp: "2026-04-24T20:00:00Z".into(),
    })
    .unwrap();
    w.append(&SessionEvent::ContractAccepted {
        contract: old.clone(),
        timestamp: "2026-04-24T20:00:01Z".into(),
    })
    .unwrap();
    w.append(&SessionEvent::TurnStarted {
        turn_id: turn_id.clone(),
        run_id: run_id.clone(),
        parent_turn: None,
        timestamp: "2026-04-24T20:00:02Z".into(),
    })
    .unwrap();
    // Amend against the OLD contract.
    w.append(&SessionEvent::ContractAmended {
        contract_id: old.id.clone(),
        turn_id: turn_id.clone(),
        delta: EffectBudgetDelta {
            apply_local: 40,
            apply_repo: 0,
            network_reads: 0,
        },
        at: "2026-04-24T20:00:03Z".into(),
    })
    .unwrap();
    w.append(&SessionEvent::TurnCommitted {
        turn_id: turn_id.clone(),
        outcome: CommitOutcome::Success,
        usage: Usage::default(),
        user_input: None,
        final_assistant: None,
        at: Some("2026-04-24T20:00:04Z".into()),
    })
    .unwrap();
    // Accept a fresh contract — overrides `last_accepted_contract`.
    w.append(&SessionEvent::ContractAccepted {
        contract: new.clone(),
        timestamp: "2026-04-24T20:00:05Z".into(),
    })
    .unwrap();
    drop(w);

    let r = JsonlReader::open(&path);
    let effective = r.last_effective_contract().unwrap().expect("contract");
    assert_eq!(effective.id, new.id);
    assert_eq!(
        effective.effect_budget.max_apply_local, 20,
        "fresh contract must not inherit amends against old id"
    );
}
