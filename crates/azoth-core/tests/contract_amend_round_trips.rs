//! β: `SessionEvent::ContractAmended` survives a JSONL write/read round
//! trip and `JsonlReader::last_effective_contract` folds the delta back
//! into the accepted contract's budget.
//!
//! Also locks the documented behaviour around "unknown variant" so that
//! a future schema addition that an older binary sees is a LOUD, known
//! failure mode rather than a silent skip. See plan §β risk #3.

use azoth_core::event_store::{JsonlReader, JsonlWriter};
use azoth_core::schemas::{
    Contract, ContractId, EffectBudget, EffectBudgetDelta, RunId, Scope, SessionEvent, TurnId,
};

fn accepted_contract() -> Contract {
    Contract {
        id: ContractId::from("ctr_round".to_string()),
        goal: "test amend round trip".into(),
        non_goals: Vec::new(),
        success_criteria: vec!["round trip works".into()],
        scope: Scope {
            include_paths: vec![".".into()],
            exclude_paths: Vec::new(),
            max_turns: Some(4),
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
fn contract_amended_event_survives_write_and_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut w = JsonlWriter::open(&path).unwrap();

    let run_id = RunId::from("run_round".to_string());
    let turn_id = TurnId::from("t_round".to_string());
    let contract = accepted_contract();

    w.append(&SessionEvent::RunStarted {
        run_id: run_id.clone(),
        contract_id: contract.id.clone(),
        timestamp: "2026-04-24T18:00:00Z".into(),
    })
    .unwrap();
    w.append(&SessionEvent::ContractAccepted {
        contract: contract.clone(),
        timestamp: "2026-04-24T18:00:01Z".into(),
    })
    .unwrap();
    w.append(&SessionEvent::TurnStarted {
        turn_id: turn_id.clone(),
        run_id: run_id.clone(),
        parent_turn: None,
        timestamp: "2026-04-24T18:00:02Z".into(),
    })
    .unwrap();
    // Amend mid-turn (between TurnStarted and TurnCommitted — invariant #7).
    w.append(&SessionEvent::ContractAmended {
        contract_id: contract.id.clone(),
        turn_id: turn_id.clone(),
        delta: EffectBudgetDelta {
            apply_local: 20,
            apply_repo: 0,
            network_reads: 0,
        },
        at: "2026-04-24T18:00:03Z".into(),
    })
    .unwrap();
    w.append(&SessionEvent::TurnCommitted {
        turn_id: turn_id.clone(),
        outcome: azoth_core::schemas::CommitOutcome::Success,
        usage: azoth_core::schemas::Usage::default(),
        user_input: None,
        final_assistant: None,
        at: Some("2026-04-24T18:00:04Z".into()),
    })
    .unwrap();
    drop(w);

    let r = JsonlReader::open(&path);

    // The replayable projection yields the ContractAmended event
    // because its turn committed (invariant #7 upheld by the writer
    // above: one terminal marker follows the amend).
    let scan = r.scan().unwrap();
    let amend_evs: Vec<_> = scan
        .replayable()
        .into_iter()
        .filter(|re| matches!(re.0, SessionEvent::ContractAmended { .. }))
        .collect();
    assert_eq!(amend_evs.len(), 1, "one ContractAmended expected");

    // last_effective_contract folds the delta into the budget.
    let effective = r.last_effective_contract().unwrap().expect("contract");
    assert_eq!(effective.effect_budget.max_apply_local, 40);
    assert_eq!(effective.effect_budget.max_apply_repo, 5, "untouched class");

    // last_accepted_contract is the untouched original.
    let accepted = r.last_accepted_contract().unwrap().expect("contract");
    assert_eq!(accepted.effect_budget.max_apply_local, 20);
}

#[test]
fn contract_amended_with_empty_at_omits_field_from_wire() {
    // `at` has `skip_serializing_if = String::is_empty` so a writer that
    // leaves it blank must not leak `"at": ""` into the JSONL — cache-
    // prefix stability (see pattern_serde_skip_serializing_if_for_cache_
    // stability in project memory).
    let ev = SessionEvent::ContractAmended {
        contract_id: ContractId::from("ctr_x".to_string()),
        turn_id: TurnId::from("t_x".to_string()),
        delta: EffectBudgetDelta {
            apply_local: 5,
            apply_repo: 0,
            network_reads: 0,
        },
        at: String::new(),
    };
    let s = serde_json::to_string(&ev).unwrap();
    assert!(!s.contains("\"at\""), "wire leaked empty at: {s}");
    let back: SessionEvent = serde_json::from_str(&s).unwrap();
    assert_eq!(back, ev);
}

#[test]
fn unknown_event_variant_is_a_loud_parse_error_not_a_silent_skip() {
    // β risk #3: a future binary writes a variant our current reader
    // does not know. The documented behaviour is that `scan()` returns
    // `ProjectionError::Parse { line, .. }` — the caller SEES the drop,
    // and a replay consumer knows it is reading a log newer than itself
    // rather than silently losing history.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    std::fs::write(
        &path,
        r#"{"type":"run_started","run_id":"run_x","contract_id":"c_x","timestamp":"2026-04-24T19:00:00Z"}
{"type":"definitely_from_the_future","turn_id":"t_x","whatever":42}
"#,
    )
    .unwrap();
    let r = JsonlReader::open(&path);
    let result = r.scan();
    assert!(
        result.is_err(),
        "unknown variant must produce a Parse error, not a silent skip"
    );
    if let Err(e) = result {
        let msg = format!("{e}");
        // `Parse` carries the line number — anything else would be a
        // silent skip, which we do NOT want. Line 2 is the future
        // variant.
        assert!(
            msg.contains("line 2") || msg.to_lowercase().contains("parse"),
            "expected a Parse error on line 2, got: {msg}"
        );
    }
}
