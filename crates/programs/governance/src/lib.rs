// ============================================================================
// AETHER GOVERNANCE - On-Chain Governance
// ============================================================================
// PURPOSE: Democratic protocol upgrades and parameter changes
//
// PROCESS:
// 1. Proposal creation (requires SWR stake)
// 2. Voting period (7 days)
// 3. Quorum check (20% of staked SWR)
// 4. Execution (if passed)
//
// PROPOSAL TYPES:
// - Parameter change (fee rates, gas costs, etc.)
// - Protocol upgrade (smart contract deployment)
// - Treasury allocation (fund grants, development)
// - Emergency actions (pause, unpause)
//
// VOTING POWER:
// - 1 SWR staked = 1 vote
// - Delegation supported
// - Vote locking during voting period
//
// SECURITY:
// - Timelock (48 hours) before execution
// - Veto power for security council (optional)
// - Min proposal stake (1000 SWR)
// ============================================================================

use aether_types::{Address, H256};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProposalStatus {
    Active,    // Voting in progress
    Passed,    // Quorum reached, waiting timelock
    Failed,    // Didn't reach quorum or majority voted no
    Executed,  // Successfully executed
    Cancelled, // Cancelled by proposer
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ProposalType {
    ParameterChange { parameter: String, value: u128 },
    ProtocolUpgrade { code_hash: H256 },
    TreasuryAllocation { recipient: Address, amount: u128 },
    EmergencyAction { action: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Proposal {
    pub proposal_id: H256,
    pub proposer: Address,
    pub proposal_type: ProposalType,
    pub description: String,
    pub votes_for: u128,
    pub votes_against: u128,
    pub status: ProposalStatus,
    pub start_slot: u64,
    pub end_slot: u64,
    pub execution_slot: Option<u64>,
    pub voters: HashMap<Address, bool>, // address -> voted_for
    /// Snapshot of effective voting power at proposal creation time.
    /// Prevents flash-delegation attacks where power is moved after proposal starts.
    pub power_snapshot: HashMap<Address, u128>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GovernanceState {
    pub proposals: HashMap<H256, Proposal>,
    pub voting_power: HashMap<Address, u128>, // Derived from staking
    /// Delegation: delegator → delegate (revocable).
    pub delegations: HashMap<Address, Address>,
    /// Effective voting power after delegation aggregation.
    pub effective_power: HashMap<Address, u128>,
    pub min_proposal_stake: u128,
    pub quorum_percentage: u8, // e.g., 20 = 20%
    pub voting_period_slots: u64,
    pub timelock_slots: u64,
    pub total_voting_power: u128,
    /// On-chain treasury balance (SWR).
    pub treasury_balance: u128,
}

impl GovernanceState {
    pub fn new() -> Self {
        GovernanceState {
            proposals: HashMap::new(),
            voting_power: HashMap::new(),
            delegations: HashMap::new(),
            effective_power: HashMap::new(),
            min_proposal_stake: 1_000_000_000_000, // 1000 SWR
            quorum_percentage: 20,
            voting_period_slots: 100_800, // 7 days
            timelock_slots: 96_000,       // 48 hours
            total_voting_power: 0,
            treasury_balance: 0,
        }
    }

    /// Create a new proposal
    pub fn propose(
        &mut self,
        proposal_id: H256,
        proposer: Address,
        proposal_type: ProposalType,
        description: String,
        current_slot: u64,
    ) -> Result<(), String> {
        // Check voting power
        let voting_power = self.voting_power.get(&proposer).copied().unwrap_or(0);
        if voting_power < self.min_proposal_stake {
            return Err("insufficient voting power".to_string());
        }

        // Check proposal doesn't exist
        if self.proposals.contains_key(&proposal_id) {
            return Err("proposal already exists".to_string());
        }

        let proposal = Proposal {
            proposal_id,
            proposer,
            proposal_type,
            description,
            votes_for: 0,
            votes_against: 0,
            status: ProposalStatus::Active,
            start_slot: current_slot,
            end_slot: current_slot
                .checked_add(self.voting_period_slots)
                .ok_or_else(|| "slot overflow in voting period calculation".to_string())?,
            execution_slot: None,
            voters: HashMap::new(),
            // Snapshot effective voting power at proposal creation to prevent
            // flash-delegation attacks (delegate→vote→undelegate→vote-again).
            power_snapshot: self.effective_power.clone(),
        };

        self.proposals.insert(proposal_id, proposal);

        Ok(())
    }

    /// Cast a vote
    pub fn vote(
        &mut self,
        proposal_id: H256,
        voter: Address,
        vote_for: bool,
        current_slot: u64,
    ) -> Result<(), String> {
        let proposal = self
            .proposals
            .get_mut(&proposal_id)
            .ok_or("proposal not found")?;

        // Check proposal is active
        if proposal.status != ProposalStatus::Active {
            return Err("proposal not active".to_string());
        }

        // Check voting period
        if current_slot < proposal.start_slot || current_slot > proposal.end_slot {
            return Err("not in voting period".to_string());
        }

        // Check already voted
        if proposal.voters.contains_key(&voter) {
            return Err("already voted".to_string());
        }

        // Use the power snapshot from proposal creation time to prevent
        // flash-delegation attacks (delegate→vote→undelegate→vote-again).
        let power = proposal.power_snapshot.get(&voter).copied().unwrap_or(0);
        if power == 0 {
            return Err("no voting power (at proposal creation time)".to_string());
        }

        // Record vote (1x conviction by default)
        proposal.voters.insert(voter, vote_for);
        if vote_for {
            proposal.votes_for = proposal
                .votes_for
                .checked_add(power)
                .ok_or("votes_for overflow")?;
        } else {
            proposal.votes_against = proposal
                .votes_against
                .checked_add(power)
                .ok_or("votes_against overflow")?;
        }

        Ok(())
    }

    /// Finalize proposal (after voting period)
    pub fn finalize(&mut self, proposal_id: H256, current_slot: u64) -> Result<(), String> {
        let proposal = self
            .proposals
            .get_mut(&proposal_id)
            .ok_or("proposal not found")?;

        if proposal.status != ProposalStatus::Active {
            return Err("proposal not active".to_string());
        }

        if current_slot <= proposal.end_slot {
            return Err("voting period not ended".to_string());
        }

        // Check quorum
        if self.quorum_percentage > 100 {
            return Err(format!(
                "quorum_percentage {} exceeds 100 — governance is permanently frozen",
                self.quorum_percentage
            ));
        }
        let total_votes = proposal
            .votes_for
            .checked_add(proposal.votes_against)
            .ok_or("total_votes overflow")?;
        let quorum_threshold = self
            .total_voting_power
            .checked_mul(self.quorum_percentage as u128)
            .ok_or("quorum threshold overflow")?
            / 100;
        if quorum_threshold == 0 {
            return Err("quorum is zero: no voting power registered".to_string());
        }

        if total_votes < quorum_threshold {
            proposal.status = ProposalStatus::Failed;
            return Ok(());
        }

        // Check majority
        if proposal.votes_for > proposal.votes_against {
            proposal.status = ProposalStatus::Passed;
            proposal.execution_slot = Some(
                current_slot
                    .checked_add(self.timelock_slots)
                    .ok_or_else(|| "slot overflow in timelock calculation".to_string())?,
            );
        } else {
            proposal.status = ProposalStatus::Failed;
        }

        Ok(())
    }

    /// Execute a passed proposal
    pub fn execute(
        &mut self,
        proposal_id: H256,
        current_slot: u64,
    ) -> Result<ProposalType, String> {
        let proposal = self
            .proposals
            .get_mut(&proposal_id)
            .ok_or("proposal not found")?;

        if proposal.status != ProposalStatus::Passed {
            return Err("proposal not passed".to_string());
        }

        // Check timelock
        if let Some(execution_slot) = proposal.execution_slot {
            if current_slot < execution_slot {
                return Err("timelock not expired".to_string());
            }
        } else {
            return Err("execution slot not set".to_string());
        }

        proposal.status = ProposalStatus::Executed;

        Ok(proposal.proposal_type.clone())
    }

    /// Cancel a proposal (by proposer)
    pub fn cancel(&mut self, proposal_id: H256, caller: Address) -> Result<(), String> {
        let proposal = self
            .proposals
            .get_mut(&proposal_id)
            .ok_or("proposal not found")?;

        if caller != proposal.proposer {
            return Err("not proposer".to_string());
        }

        if proposal.status != ProposalStatus::Active {
            return Err("cannot cancel proposal".to_string());
        }

        proposal.status = ProposalStatus::Cancelled;

        Ok(())
    }

    /// Update voting power (called from staking module).
    pub fn update_voting_power(&mut self, account: Address, power: u128) -> Result<(), String> {
        let old_power = self.voting_power.get(&account).copied().unwrap_or(0);
        self.total_voting_power = self
            .total_voting_power
            .checked_sub(old_power)
            .ok_or("total_voting_power underflow")?
            .checked_add(power)
            .ok_or("total_voting_power overflow")?;
        self.voting_power.insert(account, power);
        self.recompute_effective_power();
        Ok(())
    }

    // ── Liquid Delegation ──────────────────────────────────

    /// Delegate voting power to a representative (revocable at any time).
    ///
    /// The delegate votes on behalf of the delegator. The delegator's
    /// raw voting power is added to the delegate's effective power.
    ///
    /// **Note:** Delegation takes effect immediately in `effective_power`.
    /// To prevent flash-delegation attacks (delegate → vote → undelegate in
    /// the same block), proposals record a `power_snapshot` at creation time
    /// and votes are weighted by that snapshot, not current effective power.
    pub fn delegate(&mut self, delegator: Address, delegate: Address) -> Result<(), String> {
        if delegator == delegate {
            return Err("cannot delegate to self".into());
        }
        // Prevent delegation chains (A→B→C): delegate must not be delegating to someone else
        if self.delegations.contains_key(&delegate) {
            return Err("delegate is already delegating to someone else (no chains)".into());
        }
        // Prevent delegation chains in reverse: if the delegator already receives
        // delegations from others, they cannot delegate onwards (that would create
        // A→B→C where B's received power doesn't cascade to C).
        let delegator_receives = self.delegations.values().any(|d| *d == delegator);
        if delegator_receives {
            return Err("delegator already receives delegations from others (no chains)".into());
        }
        self.delegations.insert(delegator, delegate);
        self.recompute_effective_power();
        Ok(())
    }

    /// Revoke delegation (return voting power to self).
    pub fn undelegate(&mut self, delegator: Address) -> Result<(), String> {
        if self.delegations.remove(&delegator).is_none() {
            return Err("no active delegation".into());
        }
        self.recompute_effective_power();
        Ok(())
    }

    /// Recompute effective voting power after delegation changes.
    fn recompute_effective_power(&mut self) {
        // Start with raw power
        let mut effective: HashMap<Address, u128> = self.voting_power.clone();

        // Apply delegations: move delegator's power to delegate
        for (delegator, delegate) in &self.delegations {
            let power = self.voting_power.get(delegator).copied().unwrap_or(0);
            if power > 0 {
                // Remove from delegator
                *effective.entry(*delegator).or_insert(0) = 0;
                // Add to delegate
                let entry = effective.entry(*delegate).or_insert(0);
                *entry = entry.saturating_add(power);
            }
        }

        self.effective_power = effective;
    }

    /// Get effective voting power (after delegation).
    pub fn effective_voting_power(&self, account: &Address) -> u128 {
        self.effective_power.get(account).copied().unwrap_or(0)
    }

    // ── Conviction Voting ──────────────────────────────────

    /// Calculate conviction-weighted voting power.
    ///
    /// Conviction voting: longer lock duration = higher vote weight.
    /// Multiplier = 1 + (lock_slots / voting_period_slots)
    /// Capped at 6x multiplier (locking for 5 full voting periods).
    pub fn conviction_multiplier(&self, lock_slots: u64) -> u128 {
        let periods = lock_slots / self.voting_period_slots.max(1);
        let multiplier = 1 + periods.min(5); // Cap at 6x
        multiplier as u128
    }

    /// Cast a conviction-weighted vote.
    ///
    /// `lock_slots`: how many slots the voter commits to locking their stake.
    /// Higher lock = higher vote weight (up to 6x).
    pub fn vote_with_conviction(
        &mut self,
        proposal_id: H256,
        voter: Address,
        vote_for: bool,
        lock_slots: u64,
        current_slot: u64,
    ) -> Result<(), String> {
        let multiplier = self.conviction_multiplier(lock_slots);

        let proposal = self
            .proposals
            .get_mut(&proposal_id)
            .ok_or("proposal not found")?;

        if proposal.status != ProposalStatus::Active {
            return Err("proposal not active".into());
        }
        if current_slot < proposal.start_slot || current_slot > proposal.end_slot {
            return Err("not in voting period".into());
        }
        if proposal.voters.contains_key(&voter) {
            return Err("already voted".into());
        }

        let base_power = proposal.power_snapshot.get(&voter).copied().unwrap_or(0);
        if base_power == 0 {
            return Err("no voting power (at proposal creation time)".into());
        }
        let weighted_power = base_power
            .checked_mul(multiplier)
            .ok_or("weighted vote power overflow")?;

        proposal.voters.insert(voter, vote_for);
        if vote_for {
            proposal.votes_for = proposal
                .votes_for
                .checked_add(weighted_power)
                .ok_or("votes_for overflow")?;
        } else {
            proposal.votes_against = proposal
                .votes_against
                .checked_add(weighted_power)
                .ok_or("votes_against overflow")?;
        }

        Ok(())
    }

    // ── Treasury ───────────────────────────────────────────

    /// Deposit funds into the governance treasury.
    pub fn deposit_treasury(&mut self, amount: u128) -> Result<(), String> {
        self.treasury_balance = self
            .treasury_balance
            .checked_add(amount)
            .ok_or("treasury overflow")?;
        Ok(())
    }

    /// Execute a treasury allocation (after proposal passes).
    ///
    /// Decrements the treasury balance and returns `(recipient, amount)` so
    /// the caller (node / ledger layer) can credit the recipient's account.
    /// The caller **must** invoke `ledger.credit_account(recipient, amount)`
    /// with the returned values to complete the transfer.
    pub fn execute_treasury_allocation(
        &mut self,
        recipient: &Address,
        amount: u128,
    ) -> Result<(Address, u128), String> {
        if amount > self.treasury_balance {
            return Err(format!(
                "insufficient treasury balance: have {}, need {}",
                self.treasury_balance, amount
            ));
        }
        self.treasury_balance = self
            .treasury_balance
            .checked_sub(amount)
            .ok_or("treasury underflow")?;
        Ok((*recipient, amount))
    }

    pub fn get_proposal(&self, proposal_id: &H256) -> Option<&Proposal> {
        self.proposals.get(proposal_id)
    }
}

impl Default for GovernanceState {
    fn default() -> Self {
        Self::new()
    }
}
