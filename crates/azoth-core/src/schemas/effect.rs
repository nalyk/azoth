//! Effect classes, sandbox tiers, and the compile-time-exhaustive mapping
//! between them.

use super::{ArtifactId, EffectRecordId, ToolUseId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    Observe,
    Stage,
    ApplyLocal,
    ApplyRepo,
    ApplyRemoteReversible,
    ApplyRemoteStateful,
    ApplyIrreversible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SandboxTier {
    A,
    B,
    C,
    D,
}

impl From<EffectClass> for SandboxTier {
    fn from(ec: EffectClass) -> Self {
        match ec {
            EffectClass::Observe => SandboxTier::A,
            EffectClass::Stage => SandboxTier::B,
            EffectClass::ApplyLocal => SandboxTier::B,
            EffectClass::ApplyRepo => SandboxTier::B,
            EffectClass::ApplyRemoteReversible => SandboxTier::C,
            EffectClass::ApplyRemoteStateful => SandboxTier::D,
            EffectClass::ApplyIrreversible => SandboxTier::D,
        }
    }
}

impl EffectClass {
    /// Tiers C and D are architectural hooks only in v1 — any effect landing
    /// on those tiers should be rejected with `EffectNotAvailable`.
    pub fn is_available_in_v1(self) -> bool {
        matches!(
            self,
            EffectClass::Observe
                | EffectClass::Stage
                | EffectClass::ApplyLocal
                | EffectClass::ApplyRepo
        )
    }

    /// Snake-case wire name, matching the serde `rename_all` discipline.
    /// Shared by the approval sheet header and structured logs so they agree
    /// on one spelling (`apply_local`, not the `{:?}.to_lowercase()` squash
    /// `applylocal` that shipped through 2026-04-24).
    pub fn as_snake(self) -> &'static str {
        match self {
            EffectClass::Observe => "observe",
            EffectClass::Stage => "stage",
            EffectClass::ApplyLocal => "apply_local",
            EffectClass::ApplyRepo => "apply_repo",
            EffectClass::ApplyRemoteReversible => "apply_remote_reversible",
            EffectClass::ApplyRemoteStateful => "apply_remote_stateful",
            EffectClass::ApplyIrreversible => "apply_irreversible",
        }
    }
}

impl std::fmt::Display for EffectClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_snake())
    }
}

/// Per-run tally of effects consumed, indexed by effect class. Owned by the
/// TUI worker (or a test harness) across turns so the `TurnDriver` can short-
/// circuit a tool call when the contract's `EffectBudget` for that class
/// would be exceeded. On resume the worker recomputes this from the
/// replayable JSONL projection (see
/// [`JsonlReader::committed_run_progress`]); fresh sessions start at zero.
///
/// β: the three `*_ceiling_bonus` fields accumulate every granted
/// `ContractAmended` delta, so the driver's budget check reads the
/// effective ceiling as `contract.effect_budget.max_X + apply_X_ceiling_bonus`
/// without mutating the base `Contract` object through its shared reference.
/// JSONL replay rebuilds the bonuses the same way it rebuilds the
/// consumption tallies (see `committed_run_progress`) so resume observes
/// the same effective ceiling the live turn did.
///
/// `amends_this_turn` is reset to 0 each time `TurnDriver::drive_turn`
/// enters; `amends_this_run` is never reset (brake: ≤6 per run).
/// Kept `Copy` by staying all-u32 — so every existing test constructing
/// an `EffectCounter` by field literal keeps compiling via
/// `..Default::default()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EffectCounter {
    pub apply_local: u32,
    pub apply_repo: u32,
    pub network_reads: u32,
    /// β: accumulated `apply_local` delta from granted amends, folded
    /// additively into the effective ceiling.
    pub apply_local_ceiling_bonus: u32,
    /// β: accumulated `apply_repo` delta from granted amends.
    pub apply_repo_ceiling_bonus: u32,
    /// β: accumulated `network_reads` delta from granted amends.
    pub network_reads_ceiling_bonus: u32,
    /// β: amend grants observed in the currently-open turn. Reset to 0
    /// at drive_turn entry. Drives the ≤2-per-turn brake in
    /// `AuthorityEngine::authorize_budget_extension`.
    pub amends_this_turn: u32,
    /// β: amend grants observed over the whole run. Never reset.
    /// Drives the ≤6-per-run brake.
    pub amends_this_run: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_snake_case_matches_serde_rename_all() {
        // F8 2026-04-24: the approval sheet used `format!("{:?}",
        // req.effect_class).to_lowercase()` which squashes CamelCase
        // to `applylocal`. Display keeps sheet + logs + serde on one
        // spelling.
        assert_eq!(EffectClass::Observe.to_string(), "observe");
        assert_eq!(EffectClass::Stage.to_string(), "stage");
        assert_eq!(EffectClass::ApplyLocal.to_string(), "apply_local");
        assert_eq!(EffectClass::ApplyRepo.to_string(), "apply_repo");
        assert_eq!(
            EffectClass::ApplyRemoteReversible.to_string(),
            "apply_remote_reversible"
        );
        assert_eq!(
            EffectClass::ApplyRemoteStateful.to_string(),
            "apply_remote_stateful"
        );
        assert_eq!(
            EffectClass::ApplyIrreversible.to_string(),
            "apply_irreversible"
        );
        // Serde rename_all agreement check — Display and JSON must match.
        for ec in [
            EffectClass::Observe,
            EffectClass::ApplyLocal,
            EffectClass::ApplyRemoteStateful,
        ] {
            let json = serde_json::to_value(ec).unwrap();
            let wire = json.as_str().expect("EffectClass serialises as string");
            assert_eq!(wire, ec.as_snake(), "serde wire must match Display/as_snake");
        }
    }

    #[test]
    fn reset_for_new_contract_zeroes_bonus_only() {
        let mut c = EffectCounter {
            apply_local: 17,
            apply_repo: 4,
            network_reads: 2,
            apply_local_ceiling_bonus: 50,
            apply_repo_ceiling_bonus: 3,
            network_reads_ceiling_bonus: 1,
            amends_this_turn: 2,
            amends_this_run: 5,
        };
        c.reset_for_new_contract();
        // Zeroed: the contract-scoped ceiling bonuses.
        assert_eq!(c.apply_local_ceiling_bonus, 0);
        assert_eq!(c.apply_repo_ceiling_bonus, 0);
        assert_eq!(c.network_reads_ceiling_bonus, 0);
        // Preserved: run-scoped brake counter — R5 fix. Resetting
        // amends_this_run here would let a user bypass
        // MAX_AMENDS_PER_RUN by cycling contracts in a single run.
        assert_eq!(
            c.amends_this_run, 5,
            "run-scope brake MUST survive contract replacement"
        );
        // Preserved: effect tallies (pre-β scope) + per-turn counter
        // (drive_turn handles amends_this_turn via its own reset on
        // entry).
        assert_eq!(c.apply_local, 17);
        assert_eq!(c.apply_repo, 4);
        assert_eq!(c.network_reads, 2);
        assert_eq!(c.amends_this_turn, 2);
    }
}

impl EffectCounter {
    /// R4+R5 (PR #31 codex P1 ×2 + gemini MED): clear the
    /// CONTRACT-SCOPED amend state. Zeros the three
    /// `*_ceiling_bonus` fields only. Effect-count tallies
    /// (`apply_local`, `apply_repo`, `network_reads`) and both amend
    /// counters (`amends_this_turn` resets on `drive_turn` entry,
    /// `amends_this_run` is RUN-scoped) are preserved.
    ///
    /// R5 correction: my R4 draft also zeroed `amends_this_run`.
    /// That contradicted my own R2 fix in `JsonlReader::fold_progress`
    /// that explicitly preserves `amends_this_run` across
    /// `ContractAccepted` so a user can't bypass `MAX_AMENDS_PER_RUN`
    /// by cycling contracts inside a single run. Both the gemini MED
    /// thread on schemas/effect.rs:158 and the codex P1 on
    /// schemas/effect.rs:157 caught the self-contradiction.
    ///
    /// Call site contract: a worker that accepts a fresh contract
    /// mid-session MUST call this on its owned `EffectCounter`
    /// before the next `drive_turn` — otherwise amend bonuses
    /// granted under the prior contract silently inflate the new
    /// contract's effective ceiling and over-permit effects (e.g.,
    /// old contract amended by +20, new contract max=5 evaluates
    /// as 25). The replay-side equivalent is
    /// `JsonlReader::fold_progress`, which resets the same
    /// ceiling-bonus fields (and preserves `amends_this_run`) on
    /// every `ContractAccepted` it walks; this method keeps the
    /// live-driver path symmetric.
    ///
    /// Wired at `crates/azoth/src/tui/app.rs`'s `contract_rx` arm
    /// (β R5). The `/contract <goal>` path is a real mid-session
    /// replacement; my R4 memo incorrectly claimed otherwise.
    pub fn reset_for_new_contract(&mut self) {
        self.apply_local_ceiling_bonus = 0;
        self.apply_repo_ceiling_bonus = 0;
        self.network_reads_ceiling_bonus = 0;
        // amends_this_run intentionally preserved — see doc above
        // and the sibling comment in JsonlReader::fold_progress.
    }
}

/// One recorded effect against the world. Emitted on every tool dispatch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectRecord {
    pub id: EffectRecordId,
    pub tool_use_id: ToolUseId,
    pub class: EffectClass,
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_artifact: Option<ArtifactId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
