//! Agent run escrow state machine.
//!
//! This program anchors Beater ecosystem agent runs without moving the runtime
//! itself on-chain. It stores escrow accounting, journal roots, receipt hashes,
//! challenge state, and settlement/refund transitions.

use aether_agent_schema::{AgentRunId, JournalRoot, SettlementPolicy, SideEffect};
use aether_types::{Address, H256};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AgentRunEscrowError {
    #[error("run already exists")]
    RunAlreadyExists,
    #[error("run not found")]
    RunNotFound,
    #[error("escrow budget must be non-zero")]
    ZeroBudget,
    #[error("escrow budget is below settlement policy minimum")]
    BudgetBelowPolicyMinimum,
    #[error("challenge window must be non-zero")]
    ZeroChallengeWindow,
    #[error("deadline must be in the future")]
    InvalidDeadline,
    #[error("provider cannot be requester")]
    ProviderIsRequester,
    #[error("caller is not authorized for this transition")]
    Unauthorized,
    #[error("invalid run status for this transition")]
    InvalidStatus,
    #[error("receipt sequence must be greater than zero")]
    InvalidReceiptSequence,
    #[error("receipt sequence already committed")]
    DuplicateReceiptSequence,
    #[error("receipt sequence must increase monotonically")]
    NonMonotonicReceiptSequence,
    #[error("receipt hash must not be zero")]
    ZeroReceiptHash,
    #[error("evidence hash must not be zero")]
    ZeroEvidenceHash,
    #[error("challenge bond must be non-zero")]
    ZeroChallengeBond,
    #[error("challenge window has not ended")]
    ChallengeWindowActive,
    #[error("challenge window has ended")]
    ChallengeWindowEnded,
    #[error("human confirmation is required before settlement")]
    HumanConfirmationRequired,
    #[error("run deadline has not passed")]
    DeadlineActive,
    #[error("escrow accounting underflow")]
    EscrowUnderflow,
    #[error("escrow accounting overflow")]
    EscrowOverflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunEscrowStatus {
    Running,
    Closed,
    NeedsReview,
    Disputed,
    Settled,
    Refunded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepCommitment {
    pub seq: u64,
    pub receipt_hash: H256,
    pub side_effect: SideEffect,
    pub committed_slot: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dispute {
    pub seq: u64,
    pub challenger: Address,
    pub evidence_hash: H256,
    pub bond_aic: u128,
    pub opened_slot: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRun {
    pub run_id: AgentRunId,
    pub requester: Address,
    pub provider: Address,
    pub budget_aic: u128,
    pub initial_journal_root: JournalRoot,
    pub final_journal_root: Option<JournalRoot>,
    pub evidence_uri_hash: Option<H256>,
    pub policy: SettlementPolicy,
    pub status: AgentRunEscrowStatus,
    pub opened_slot: u64,
    pub deadline_slot: u64,
    pub challenge_end_slot: Option<u64>,
    pub human_confirmed: bool,
    pub steps: BTreeMap<u64, StepCommitment>,
    pub dispute: Option<Dispute>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AgentRunEscrowState {
    pub runs: HashMap<AgentRunId, AgentRun>,
    pub requester_escrow: HashMap<Address, u128>,
    pub provider_claimable: HashMap<Address, u128>,
    pub challenger_bonds: HashMap<Address, u128>,
    pub total_runs: u64,
    pub settled_runs: u64,
    pub disputed_runs: u64,
}

impl AgentRunEscrowState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn open_run(
        &mut self,
        run_id: AgentRunId,
        requester: Address,
        provider: Address,
        budget_aic: u128,
        journal_root: JournalRoot,
        policy: SettlementPolicy,
        current_slot: u64,
        deadline_slots: u64,
    ) -> Result<(), AgentRunEscrowError> {
        if self.runs.contains_key(&run_id) {
            return Err(AgentRunEscrowError::RunAlreadyExists);
        }
        if budget_aic == 0 {
            return Err(AgentRunEscrowError::ZeroBudget);
        }
        if budget_aic < policy.min_escrow_aic {
            return Err(AgentRunEscrowError::BudgetBelowPolicyMinimum);
        }
        if policy.challenge_slots == 0 {
            return Err(AgentRunEscrowError::ZeroChallengeWindow);
        }
        if deadline_slots == 0 {
            return Err(AgentRunEscrowError::InvalidDeadline);
        }
        if provider == requester {
            return Err(AgentRunEscrowError::ProviderIsRequester);
        }
        let human_confirmed = !policy.requires_human_confirm;

        let deadline_slot = current_slot
            .checked_add(deadline_slots)
            .ok_or(AgentRunEscrowError::EscrowOverflow)?;

        let run = AgentRun {
            run_id,
            requester,
            provider,
            budget_aic,
            initial_journal_root: journal_root,
            final_journal_root: None,
            evidence_uri_hash: None,
            policy,
            status: AgentRunEscrowStatus::Running,
            opened_slot: current_slot,
            deadline_slot,
            challenge_end_slot: None,
            human_confirmed,
            steps: BTreeMap::new(),
            dispute: None,
        };

        let escrowed = self.requester_escrow.entry(requester).or_insert(0);
        *escrowed = escrowed
            .checked_add(budget_aic)
            .ok_or(AgentRunEscrowError::EscrowOverflow)?;
        self.total_runs = self
            .total_runs
            .checked_add(1)
            .ok_or(AgentRunEscrowError::EscrowOverflow)?;
        self.runs.insert(run_id, run);
        Ok(())
    }

    pub fn commit_step(
        &mut self,
        run_id: AgentRunId,
        provider: Address,
        seq: u64,
        receipt_hash: H256,
        side_effect: SideEffect,
        current_slot: u64,
    ) -> Result<(), AgentRunEscrowError> {
        let run = self
            .runs
            .get_mut(&run_id)
            .ok_or(AgentRunEscrowError::RunNotFound)?;
        if run.provider != provider {
            return Err(AgentRunEscrowError::Unauthorized);
        }
        if run.status != AgentRunEscrowStatus::Running {
            return Err(AgentRunEscrowError::InvalidStatus);
        }
        if seq == 0 {
            return Err(AgentRunEscrowError::InvalidReceiptSequence);
        }
        if receipt_hash == H256::zero() {
            return Err(AgentRunEscrowError::ZeroReceiptHash);
        }
        if run.steps.contains_key(&seq) {
            return Err(AgentRunEscrowError::DuplicateReceiptSequence);
        }
        if run
            .steps
            .keys()
            .next_back()
            .is_some_and(|last| seq <= *last)
        {
            return Err(AgentRunEscrowError::NonMonotonicReceiptSequence);
        }

        run.steps.insert(
            seq,
            StepCommitment {
                seq,
                receipt_hash,
                side_effect,
                committed_slot: current_slot,
            },
        );
        Ok(())
    }

    pub fn close_run(
        &mut self,
        run_id: AgentRunId,
        provider: Address,
        final_journal_root: JournalRoot,
        evidence_uri_hash: H256,
        current_slot: u64,
    ) -> Result<(), AgentRunEscrowError> {
        if evidence_uri_hash == H256::zero() {
            return Err(AgentRunEscrowError::ZeroEvidenceHash);
        }
        let run = self
            .runs
            .get_mut(&run_id)
            .ok_or(AgentRunEscrowError::RunNotFound)?;
        if run.provider != provider {
            return Err(AgentRunEscrowError::Unauthorized);
        }
        if run.status != AgentRunEscrowStatus::Running {
            return Err(AgentRunEscrowError::InvalidStatus);
        }

        run.final_journal_root = Some(final_journal_root);
        run.evidence_uri_hash = Some(evidence_uri_hash);
        run.status = AgentRunEscrowStatus::Closed;
        run.challenge_end_slot = Some(
            current_slot
                .checked_add(run.policy.challenge_slots)
                .ok_or(AgentRunEscrowError::EscrowOverflow)?,
        );
        Ok(())
    }

    pub fn mark_needs_review(
        &mut self,
        run_id: AgentRunId,
        caller: Address,
    ) -> Result<(), AgentRunEscrowError> {
        let run = self
            .runs
            .get_mut(&run_id)
            .ok_or(AgentRunEscrowError::RunNotFound)?;
        if caller != run.provider && caller != run.requester {
            return Err(AgentRunEscrowError::Unauthorized);
        }
        if run.status != AgentRunEscrowStatus::Running && run.status != AgentRunEscrowStatus::Closed
        {
            return Err(AgentRunEscrowError::InvalidStatus);
        }
        run.status = AgentRunEscrowStatus::NeedsReview;
        Ok(())
    }

    pub fn confirm_run(
        &mut self,
        run_id: AgentRunId,
        requester: Address,
    ) -> Result<(), AgentRunEscrowError> {
        let run = self
            .runs
            .get_mut(&run_id)
            .ok_or(AgentRunEscrowError::RunNotFound)?;
        if requester != run.requester {
            return Err(AgentRunEscrowError::Unauthorized);
        }
        if run.status != AgentRunEscrowStatus::Closed
            && run.status != AgentRunEscrowStatus::NeedsReview
        {
            return Err(AgentRunEscrowError::InvalidStatus);
        }
        if run.status == AgentRunEscrowStatus::NeedsReview && run.final_journal_root.is_some() {
            run.status = AgentRunEscrowStatus::Closed;
        }
        run.human_confirmed = true;
        Ok(())
    }

    pub fn dispute_step(
        &mut self,
        run_id: AgentRunId,
        seq: u64,
        challenger: Address,
        evidence_hash: H256,
        bond_aic: u128,
        current_slot: u64,
    ) -> Result<(), AgentRunEscrowError> {
        if evidence_hash == H256::zero() {
            return Err(AgentRunEscrowError::ZeroEvidenceHash);
        }
        if bond_aic == 0 {
            return Err(AgentRunEscrowError::ZeroChallengeBond);
        }
        let run = self
            .runs
            .get_mut(&run_id)
            .ok_or(AgentRunEscrowError::RunNotFound)?;
        if run.status != AgentRunEscrowStatus::Closed {
            return Err(AgentRunEscrowError::InvalidStatus);
        }
        let challenge_end = run
            .challenge_end_slot
            .ok_or(AgentRunEscrowError::InvalidStatus)?;
        if current_slot > challenge_end {
            return Err(AgentRunEscrowError::ChallengeWindowEnded);
        }
        if !run.steps.contains_key(&seq) {
            return Err(AgentRunEscrowError::InvalidReceiptSequence);
        }

        run.status = AgentRunEscrowStatus::Disputed;
        run.dispute = Some(Dispute {
            seq,
            challenger,
            evidence_hash,
            bond_aic,
            opened_slot: current_slot,
        });
        let bond = self.challenger_bonds.entry(challenger).or_insert(0);
        *bond = bond
            .checked_add(bond_aic)
            .ok_or(AgentRunEscrowError::EscrowOverflow)?;
        self.disputed_runs = self
            .disputed_runs
            .checked_add(1)
            .ok_or(AgentRunEscrowError::EscrowOverflow)?;
        Ok(())
    }

    pub fn settle_run(
        &mut self,
        run_id: AgentRunId,
        current_slot: u64,
    ) -> Result<(Address, u128), AgentRunEscrowError> {
        let (requester, provider, budget_aic) = {
            let run = self
                .runs
                .get(&run_id)
                .ok_or(AgentRunEscrowError::RunNotFound)?;
            if run.status != AgentRunEscrowStatus::Closed {
                return Err(AgentRunEscrowError::InvalidStatus);
            }
            let challenge_end = run
                .challenge_end_slot
                .ok_or(AgentRunEscrowError::InvalidStatus)?;
            if current_slot <= challenge_end {
                return Err(AgentRunEscrowError::ChallengeWindowActive);
            }
            if run.policy.requires_human_confirm && !run.human_confirmed {
                return Err(AgentRunEscrowError::HumanConfirmationRequired);
            }
            (run.requester, run.provider, run.budget_aic)
        };

        self.release_escrow_to_provider(requester, provider, budget_aic)?;
        let run = self
            .runs
            .get_mut(&run_id)
            .ok_or(AgentRunEscrowError::RunNotFound)?;
        run.status = AgentRunEscrowStatus::Settled;
        self.settled_runs = self
            .settled_runs
            .checked_add(1)
            .ok_or(AgentRunEscrowError::EscrowOverflow)?;
        Ok((provider, budget_aic))
    }

    pub fn refund_run(
        &mut self,
        run_id: AgentRunId,
        requester: Address,
        current_slot: u64,
    ) -> Result<u128, AgentRunEscrowError> {
        let budget_aic = {
            let run = self
                .runs
                .get(&run_id)
                .ok_or(AgentRunEscrowError::RunNotFound)?;
            if run.requester != requester {
                return Err(AgentRunEscrowError::Unauthorized);
            }
            if run.status != AgentRunEscrowStatus::Running
                && run.status != AgentRunEscrowStatus::NeedsReview
            {
                return Err(AgentRunEscrowError::InvalidStatus);
            }
            if current_slot <= run.deadline_slot {
                return Err(AgentRunEscrowError::DeadlineActive);
            }
            run.budget_aic
        };

        let escrowed = self
            .requester_escrow
            .get_mut(&requester)
            .ok_or(AgentRunEscrowError::EscrowUnderflow)?;
        if *escrowed < budget_aic {
            return Err(AgentRunEscrowError::EscrowUnderflow);
        }
        *escrowed = escrowed
            .checked_sub(budget_aic)
            .ok_or(AgentRunEscrowError::EscrowUnderflow)?;
        if *escrowed == 0 {
            self.requester_escrow.remove(&requester);
        }

        let run = self
            .runs
            .get_mut(&run_id)
            .ok_or(AgentRunEscrowError::RunNotFound)?;
        run.status = AgentRunEscrowStatus::Refunded;
        Ok(budget_aic)
    }

    fn release_escrow_to_provider(
        &mut self,
        requester: Address,
        provider: Address,
        amount: u128,
    ) -> Result<(), AgentRunEscrowError> {
        let escrowed = self
            .requester_escrow
            .get_mut(&requester)
            .ok_or(AgentRunEscrowError::EscrowUnderflow)?;
        if *escrowed < amount {
            return Err(AgentRunEscrowError::EscrowUnderflow);
        }
        *escrowed = escrowed
            .checked_sub(amount)
            .ok_or(AgentRunEscrowError::EscrowUnderflow)?;
        if *escrowed == 0 {
            self.requester_escrow.remove(&requester);
        }
        let claimable = self.provider_claimable.entry(provider).or_insert(0);
        *claimable = claimable
            .checked_add(amount)
            .ok_or(AgentRunEscrowError::EscrowOverflow)?;
        Ok(())
    }
}
