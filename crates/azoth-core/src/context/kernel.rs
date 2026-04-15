//! Context Kernel: compile five-lane `ContextPacket`s from durable state.

use super::tokenizer::{count_tokens, TokenizerFamily};
use crate::schemas::{
    CheckpointSummary, ConstitutionLane, Contract, ContextPacket, ContextPacketId, EvidenceItem,
    ExitCriteria, TurnId, WorkingSetItem,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum KernelError {
    #[error("serialization: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("token budget exceeded: packet {0} > limit {1}")]
    OverBudget(usize, usize),
}

pub struct ContextKernel<'a> {
    pub policy_version: &'a str,
    pub tokenizer: TokenizerFamily,
    pub max_input_tokens: usize,
}

pub struct StepInput<'a> {
    pub contract: &'a Contract,
    pub turn_id: TurnId,
    pub step_goal: String,
    pub rubric: Vec<String>,
    pub working_set: Vec<WorkingSetItem>,
    pub evidence: Vec<EvidenceItem>,
    pub last_checkpoint: Option<CheckpointSummary>,
    pub system_prompt: String,
    pub tool_schemas_digest: String,
}

impl<'a> ContextKernel<'a> {
    pub fn compile(&self, input: StepInput<'a>) -> Result<ContextPacket, KernelError> {
        let mut evidence = input.evidence;
        // Critical-first ordering: sort by decision_weight desc.
        evidence.sort_by(|a, b| b.decision_weight.cmp(&a.decision_weight));

        let constitution = ConstitutionLane {
            contract_digest: digest_json(input.contract)?,
            tool_schemas_digest: input.tool_schemas_digest,
            policy_version: self.policy_version.to_string(),
            system_prompt: input.system_prompt,
        };

        let mut packet = ContextPacket {
            id: ContextPacketId::new(),
            contract_id: input.contract.id.clone(),
            turn_id: input.turn_id,
            digest: String::new(), // filled below
            constitution_lane: constitution,
            working_set_lane: input.working_set,
            evidence_lane: evidence,
            checkpoint_lane: input.last_checkpoint,
            exit_criteria_lane: ExitCriteria {
                step_goal: input.step_goal,
                rubric: input.rubric,
            },
        };

        // Final digest is computed over the full packet.
        packet.digest = digest_json(&packet)?;

        let approx = self.approximate_tokens(&packet);
        if self.max_input_tokens > 0 && approx > self.max_input_tokens {
            return Err(KernelError::OverBudget(approx, self.max_input_tokens));
        }

        Ok(packet)
    }

    fn approximate_tokens(&self, packet: &ContextPacket) -> usize {
        let ser = serde_json::to_string(packet).unwrap_or_default();
        count_tokens(&ser, self.tokenizer)
    }
}

fn digest_json<T: serde::Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let bytes = serde_json::to_vec(value)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::{Contract, ContractId, EffectBudget, Scope};

    fn sample_contract() -> Contract {
        Contract {
            id: ContractId::from("ctr_1".to_string()),
            goal: "fix bug".into(),
            non_goals: vec![],
            success_criteria: vec!["tests pass".into()],
            scope: Scope::default(),
            effect_budget: EffectBudget::default(),
            notes: vec![],
        }
    }

    #[test]
    fn kernel_compiles_packet_with_sorted_evidence() {
        let kernel = ContextKernel {
            policy_version: "policy_v1",
            tokenizer: TokenizerFamily::Anthropic,
            max_input_tokens: 0,
        };
        let contract = sample_contract();
        let input = StepInput {
            contract: &contract,
            turn_id: TurnId::from("t_1".to_string()),
            step_goal: "find the off-by-one".into(),
            rubric: vec!["tests green".into()],
            working_set: vec![],
            evidence: vec![
                EvidenceItem {
                    label: "low".into(),
                    artifact_ref: None,
                    inline: Some("a".into()),
                    decision_weight: 1,
                },
                EvidenceItem {
                    label: "high".into(),
                    artifact_ref: None,
                    inline: Some("b".into()),
                    decision_weight: 100,
                },
            ],
            last_checkpoint: None,
            system_prompt: "you are azoth".into(),
            tool_schemas_digest: "sha256:deadbeef".into(),
        };
        let packet = kernel.compile(input).unwrap();
        assert_eq!(packet.evidence_lane[0].label, "high");
        assert!(packet.digest.starts_with("sha256:"));
    }
}
