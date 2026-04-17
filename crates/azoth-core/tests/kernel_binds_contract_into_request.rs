//! ContextKernel wiring — the persisted contract must influence the
//! outgoing `ModelRequest.request_digest` through the constitution lane.
//!
//! Two turns run back-to-back against identical state and adapter scripts:
//! one with `kernel: None` (no compile), one with `kernel: Some(&k)`
//! (compile shadows `system` with a constitution header). Because the
//! `ModelTurnRequest` digest hashes the `system` field, the two recorded
//! `request_digest`s MUST differ — and the kernel-compiled one must be
//! deterministic across repeated runs with the same contract.

use azoth_core::adapter::{MockAdapter, MockScript, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{ApprovalRequestMsg, CapabilityStore};
use azoth_core::context::{ContextKernel, TokenizerFamily};
use azoth_core::contract;
use azoth_core::event_store::{JsonlReader, JsonlWriter};
use azoth_core::execution::{ExecutionContext, ToolDispatcher};
use azoth_core::schemas::{
    ContentBlock, Contract, Message, ModelTurnResponse, RunId, SessionEvent, StopReason, TurnId,
    Usage,
};
use azoth_core::turn::TurnDriver;
use tempfile::tempdir;
use tokio::sync::mpsc;

fn mock_end_turn_only() -> MockScript {
    MockScript {
        turns: vec![ModelTurnResponse {
            content: vec![ContentBlock::Text {
                text: "done".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 4,
                output_tokens: 2,
                ..Default::default()
            },
        }],
    }
}

fn first_model_request_digest(session_path: &std::path::Path) -> String {
    let forensic = JsonlReader::open(session_path)
        .forensic()
        .expect("forensic read ok");
    for entry in forensic.iter() {
        if let SessionEvent::ModelRequest { request_digest, .. } = &entry.event {
            return request_digest.clone();
        }
    }
    panic!("no ModelRequest event in {}", session_path.display());
}

async fn drive_one_turn(
    session_path: &std::path::Path,
    artifacts_root: &std::path::Path,
    repo_root: &std::path::Path,
    kernel: Option<&ContextKernel<'_>>,
    contract_ref: &Contract,
) {
    let mut writer = JsonlWriter::open(session_path).unwrap();
    let artifacts = ArtifactStore::open(artifacts_root).unwrap();
    let dispatcher = ToolDispatcher::new();
    let adapter = MockAdapter::new(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
        mock_end_turn_only(),
    );
    let run_id = RunId::from("run_kernel".to_string());
    let turn_id = TurnId::from("t_kernel_1".to_string());
    let ctx = ExecutionContext::builder(
        run_id.clone(),
        turn_id.clone(),
        artifacts,
        repo_root.to_path_buf(),
    )
    .build();
    let (approval_tx, _approval_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
    let mut caps = CapabilityStore::new();
    let mut effects = azoth_core::schemas::EffectCounter::default();

    {
        let mut driver = TurnDriver {
            run_id: run_id.clone(),
            adapter: &adapter,
            dispatcher: &dispatcher,
            writer: &mut writer,
            ctx: &ctx,
            capabilities: &mut caps,
            approval_bridge: approval_tx,
            contract: Some(contract_ref),
            turns_completed: 0,
            kernel,
            validators: &[],
            effects_consumed: &mut effects,
            evidence_collector: None,
            impact_validators: &[],
            diff_source: None,
        };
        driver
            .drive_turn(
                turn_id,
                "you are azoth".into(),
                vec![Message::user_text("go")],
            )
            .await
            .expect("turn drives cleanly");
    }
    drop(writer);
}

#[tokio::test]
async fn kernel_shadows_system_and_changes_request_digest() {
    // A single shared contract, persisted once into its own JSONL so each
    // test sub-run rehydrates the same bytes.
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let artifacts_root = repo_root.join(".azoth/artifacts");

    // Build + persist the contract in a throwaway session so that the
    // contract bytes are identical across both sub-runs (the draft helper
    // generates a fresh ContractId each call).
    let seed_path = repo_root.join(".azoth/sessions/seed.jsonl");
    let mut seed = JsonlWriter::open(&seed_path).unwrap();
    let mut drafted = contract::draft("bind contract into request");
    drafted.success_criteria.push("digest flows in".into());
    let persisted =
        contract::accept_and_persist(&mut seed, drafted, "2026-04-15T00:00:00Z".to_string())
            .expect("persist ok");
    drop(seed);

    // Run A: no kernel — baseline request_digest with just the raw system.
    let path_a = repo_root.join(".azoth/sessions/run_a.jsonl");
    drive_one_turn(&path_a, &artifacts_root, &repo_root, None, &persisted).await;
    let digest_a = first_model_request_digest(&path_a);

    // Run B: kernel attached — drive_turn shadows `system` with a
    // constitution header carrying contract_digest + policy_version.
    let kernel = ContextKernel {
        policy_version: "policy_v1",
        tokenizer: TokenizerFamily::Anthropic,
        max_input_tokens: 0,
    };
    let path_b = repo_root.join(".azoth/sessions/run_b.jsonl");
    drive_one_turn(
        &path_b,
        &artifacts_root,
        &repo_root,
        Some(&kernel),
        &persisted,
    )
    .await;
    let digest_b = first_model_request_digest(&path_b);

    assert_ne!(
        digest_a, digest_b,
        "kernel-compiled system must change request_digest vs raw system"
    );
    assert!(digest_a.starts_with("sha256:"));
    assert!(digest_b.starts_with("sha256:"));

    // Run C: rerun with the same kernel + contract — the digest must be
    // deterministic across runs so the contract binding is reproducible.
    let path_c = repo_root.join(".azoth/sessions/run_c.jsonl");
    drive_one_turn(
        &path_c,
        &artifacts_root,
        &repo_root,
        Some(&kernel),
        &persisted,
    )
    .await;
    let digest_c = first_model_request_digest(&path_c);
    assert_eq!(
        digest_b, digest_c,
        "kernel-compiled request_digest must be deterministic"
    );
}

#[tokio::test]
async fn kernel_without_contract_is_a_noop() {
    // `drive_turn` branches on `(contract, kernel)` — kernel alone, with no
    // contract, must leave the system string untouched. We prove it by
    // comparing request_digest to a run with `kernel: None`.
    let dir = tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let artifacts_root = repo_root.join(".azoth/artifacts");

    let kernel = ContextKernel {
        policy_version: "policy_v1",
        tokenizer: TokenizerFamily::Anthropic,
        max_input_tokens: 0,
    };

    async fn run_one(
        path: &std::path::Path,
        artifacts_root: &std::path::Path,
        repo_root: &std::path::Path,
        kernel: Option<&ContextKernel<'_>>,
    ) -> String {
        let mut writer = JsonlWriter::open(path).unwrap();
        let artifacts = ArtifactStore::open(artifacts_root).unwrap();
        let dispatcher = ToolDispatcher::new();
        let adapter = MockAdapter::new(
            ProviderProfile::anthropic_default("claude-sonnet-4-6"),
            mock_end_turn_only(),
        );
        let run_id = RunId::from("run_noop".to_string());
        let turn_id = TurnId::from("t_noop_1".to_string());
        let ctx = ExecutionContext::builder(
            run_id.clone(),
            turn_id.clone(),
            artifacts,
            repo_root.to_path_buf(),
        )
        .build();
        let (approval_tx, _approval_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
        let mut caps = CapabilityStore::new();
        let mut effects = azoth_core::schemas::EffectCounter::default();
        {
            let mut driver = TurnDriver {
                run_id: run_id.clone(),
                adapter: &adapter,
                dispatcher: &dispatcher,
                writer: &mut writer,
                ctx: &ctx,
                capabilities: &mut caps,
                approval_bridge: approval_tx,
                contract: None,
                turns_completed: 0,
                kernel,
                validators: &[],
                effects_consumed: &mut effects,
                evidence_collector: None,
                impact_validators: &[],
                diff_source: None,
            };
            driver
                .drive_turn(
                    turn_id,
                    "you are azoth".into(),
                    vec![Message::user_text("go")],
                )
                .await
                .expect("drives");
        }
        drop(writer);
        first_model_request_digest(path)
    }

    let path_none = repo_root.join(".azoth/sessions/none.jsonl");
    let path_kernel = repo_root.join(".azoth/sessions/kernel.jsonl");
    let d_none = run_one(&path_none, &artifacts_root, &repo_root, None).await;
    let d_kernel = run_one(&path_kernel, &artifacts_root, &repo_root, Some(&kernel)).await;
    assert_eq!(
        d_none, d_kernel,
        "kernel without contract must not alter request_digest"
    );
}
