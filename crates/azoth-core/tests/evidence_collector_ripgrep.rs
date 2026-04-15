//! Integration smoke for picklist #5: `LexicalEvidenceCollector` composed
//! with the real `RipgrepLexicalRetrieval`, feeding into `ContextKernel`.
//! Mirrors the test-only landing of #4 (9623a45) — zero production-path
//! wiring, but proves the collector works end-to-end against a real repo
//! tree and that the resulting `EvidenceItem`s survive kernel sorting.
//!
//! Uses `.ignore` (not `.gitignore`) so the `ignore::WalkBuilder` honors
//! it outside a git work tree — see memory
//! `pattern_ignore_crate_gitignore_vs_ignore.md`.

use azoth_core::context::{
    ContextKernel, EvidenceCollector, LexicalEvidenceCollector, StepInput, TokenizerFamily,
};
use azoth_core::retrieval::RipgrepLexicalRetrieval;
use azoth_core::schemas::{Contract, ContractId, EffectBudget, Scope, TurnId};
use std::sync::Arc;
use tempfile::TempDir;

fn seed_repo() -> (TempDir, std::path::PathBuf) {
    let td = TempDir::new().expect("tempdir");
    let root = td.path().to_path_buf();
    std::fs::write(root.join(".ignore"), "junk.log\n").unwrap();
    std::fs::write(
        root.join("lib.rs"),
        "fn parse() {}\nlet lantern = 42;\nprintln!(\"done\");\n",
    )
    .unwrap();
    std::fs::write(
        root.join("README.md"),
        "# readme\n\nthe lantern glows softly\n",
    )
    .unwrap();
    std::fs::write(root.join("junk.log"), "lantern in junk file\n").unwrap();
    (td, root)
}

#[tokio::test]
async fn collector_feeds_kernel_with_weighted_evidence() {
    let (_td, root) = seed_repo();
    let retrieval = Arc::new(RipgrepLexicalRetrieval { root });
    let collector = LexicalEvidenceCollector::new(retrieval);

    let evidence = collector
        .collect("lantern", 8)
        .await
        .expect("collect succeeds");
    assert!(
        !evidence.is_empty(),
        "expected at least one lantern hit from seeded repo"
    );
    // `.ignore` filtered `junk.log` out.
    assert!(
        !evidence.iter().any(|e| e.label.contains("junk.log")),
        "junk.log should be gitignored, got {:?}",
        evidence
    );
    // Label shape is "path:line".
    for item in &evidence {
        assert!(
            item.label.contains(':'),
            "label missing path:line shape: {:?}",
            item.label
        );
        assert!(item.inline.is_some(), "inline snippet expected");
        assert!(item.artifact_ref.is_none(), "v1 emits no artifact refs");
        assert!(item.decision_weight >= 1, "weight floor not enforced");
    }
    // Weights strictly descending in retrieval order.
    for pair in evidence.windows(2) {
        assert!(
            pair[0].decision_weight > pair[1].decision_weight,
            "weights must strictly decrease: {:?}",
            evidence
        );
    }

    // Feed through the kernel and confirm the evidence_lane survives
    // compile + sort without reordering (weights are already descending).
    let contract = Contract {
        id: ContractId::from("ctr_evidence".to_string()),
        goal: "locate lantern references".into(),
        non_goals: vec![],
        success_criteria: vec!["finds lib.rs hit".into()],
        scope: Scope::default(),
        effect_budget: EffectBudget::default(),
        notes: vec![],
    };
    let kernel = ContextKernel {
        policy_version: "v1",
        tokenizer: TokenizerFamily::Anthropic,
        max_input_tokens: 0,
    };
    let input = StepInput {
        contract: &contract,
        turn_id: TurnId::from("t_evidence".to_string()),
        step_goal: contract.goal.clone(),
        rubric: contract.success_criteria.clone(),
        working_set: vec![],
        evidence: evidence.clone(),
        last_checkpoint: None,
        system_prompt: "you are azoth".into(),
        tool_schemas_digest: "sha256:0".into(),
    };
    let packet = kernel.compile(input).expect("kernel compile");

    assert_eq!(packet.evidence_lane.len(), evidence.len());
    // Sort was a no-op: first lane item matches first collector item.
    assert_eq!(packet.evidence_lane[0].label, evidence[0].label);
    assert!(packet.digest.starts_with("sha256:"));
}

#[tokio::test]
async fn empty_query_bypasses_retrieval() {
    let (_td, root) = seed_repo();
    let retrieval = Arc::new(RipgrepLexicalRetrieval { root });
    let collector = LexicalEvidenceCollector::new(retrieval);
    let evidence = collector.collect("", 8).await.expect("collect");
    assert!(evidence.is_empty());
}
