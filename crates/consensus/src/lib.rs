// ============================================================================
// AETHER CONSENSUS - Full consensus implementation
// ============================================================================
// PURPOSE: Provides multiple consensus engines:
// - SimpleConsensus: Round-robin for testing
// - VRF-PoS: VRF-based leader election
// - HotStuff: BFT consensus with BLS aggregation
// - HybridConsensus: VRF + HotStuff + BLS (full Phase 1 integration)
// ============================================================================

use aether_crypto_vrf::VrfProof;
pub use aether_crypto_vrf::{VrfSigner, VrfVerifier};
use aether_types::{Block, PublicKey, Slot, Vote, H256};
use anyhow::Result;

/// Finality gadget interface — separated from consensus so alternative
/// finality mechanisms can be tested or composed independently.
///
/// # Contract
///
/// `check_finality(slot)` returns `true` **at most once** per slot.
/// Callers rely on this for one-time side effects (epoch randomness
/// rotation, finality metrics, fork-choice finalization). Implementations
/// must track which slots have already been reported and never re-report.
pub trait Finality {
    /// Returns `true` if `slot` is newly finalized and has not been
    /// reported before. Subsequent calls with the same slot return `false`.
    fn check_finality(&mut self, slot: Slot) -> bool;
    fn finalized_slot(&self) -> Slot;
    fn record_block(&mut self, _block_hash: H256, _parent_hash: H256, _slot: Slot) {}
}

/// Unified interface for all consensus engines.
/// Extends `Finality` so callers can access finality methods through `dyn ConsensusEngine`.
pub trait ConsensusEngine: Finality + Send + Sync {
    fn current_slot(&self) -> Slot;
    fn advance_slot(&mut self);

    fn skip_to_slot(&mut self, slot: Slot) {
        while self.current_slot() < slot {
            self.advance_slot();
        }
    }

    fn is_leader(&self, slot: Slot, validator_pubkey: &PublicKey) -> bool;
    fn validate_block(&self, block: &Block) -> Result<()>;
    fn add_vote(&mut self, vote: Vote) -> Result<()>;
    fn total_stake(&self) -> u128;

    fn get_leader_proof(&self, _slot: Slot) -> Option<VrfProof> {
        None
    }

    fn update_epoch_randomness(&mut self, _vrf_output: &[u8; 32]) -> bool {
        false
    }

    fn validator_stake(&self, _address: &aether_types::Address) -> u128 {
        0
    }

    fn is_timed_out(&self) -> bool {
        false
    }

    fn on_timeout(&mut self) {}

    fn advance_pacemaker_to_round(&mut self, _round: u64) {}

    fn get_bls_pubkey(&self, _address: &aether_types::Address) -> Option<Vec<u8>> {
        None
    }

    fn register_bls_pubkey(
        &mut self,
        _address: aether_types::Address,
        _bls_pubkey: Vec<u8>,
        _pop_signature: &[u8],
    ) -> Result<()> {
        Ok(())
    }

    fn slash_validator(&mut self, _address: &aether_types::Address, _slash_bps: u128) -> u128 {
        0
    }

    fn validator_addresses_and_stakes(&self) -> Vec<(aether_types::Address, u128)> {
        Vec::new()
    }

    /// Record a quorum certificate that arrived embedded in a block (the node
    /// has already verified its BLS aggregate and stake). Engines that track
    /// certified blocks update their lock / finality from it.
    fn on_qc(&mut self, _qc: &aether_types::AggregatedVote) {}

    /// Validity checks for accepting/executing a proposal (leader proof,
    /// slot bounds) WITHOUT the voting lock. Ancestors of certified blocks
    /// must be accepted during catch-up even if they are below our lock.
    fn verify_proposal(&self, block: &Block) -> Result<()> {
        self.validate_block(block)
    }

    /// Engines without a voting-based finality gadget (the round-robin
    /// `SimpleConsensus` used in single-node tests) treat every executed
    /// block as final immediately.
    fn finalizes_on_execution(&self) -> bool {
        false
    }

    /// HotStuff safe-node rule: may this validator vote for `block`?
    fn safe_to_vote(&self, _block: &Block) -> bool {
        true
    }

    /// The quorum certificate this engine knows for `block_hash`, if any, in
    /// the wire format that is embedded in child blocks.
    fn qc_for(&self, _block_hash: &H256) -> Option<aether_types::AggregatedVote> {
        None
    }

    /// Highest-slot certified block known to this engine.
    fn high_qc(&self) -> Option<(H256, Slot)> {
        None
    }

    /// Hash of the most recently finalized block, for engines that know it.
    fn finalized_block(&self) -> Option<H256> {
        None
    }
}

/// Trivial finality for testing: every slot is immediately final.
pub struct InstantFinality {
    finalized: Slot,
}

impl InstantFinality {
    pub fn new() -> Self {
        Self { finalized: 0 }
    }
}

impl Default for InstantFinality {
    fn default() -> Self {
        Self::new()
    }
}

impl Finality for InstantFinality {
    fn check_finality(&mut self, slot: Slot) -> bool {
        if slot > self.finalized {
            self.finalized = slot;
            true
        } else {
            false
        }
    }

    fn finalized_slot(&self) -> Slot {
        self.finalized
    }
}

/// Check if `voted_stake` represents a 2/3 quorum of `total_stake`.
/// Uses checked arithmetic to avoid overflow when stake values are very large.
pub fn has_quorum(voted_stake: u128, total_stake: u128) -> bool {
    if total_stake == 0 {
        return false;
    }
    // voted_stake * 3 >= total_stake * 2
    // Use checked_mul to prevent overflow
    match (voted_stake.checked_mul(3), total_stake.checked_mul(2)) {
        (Some(lhs), Some(rhs)) => lhs >= rhs,
        // If voted_stake*3 overflows, it's definitely >= total_stake*2
        // (since voted_stake <= total_stake, total_stake*2 can't overflow if voted_stake*3 does...
        //  but to be safe, use division fallback)
        _ => {
            // Fallback: use division to avoid overflow
            // voted_stake >= total_stake * 2 / 3
            // This rounds down, so check with +1 for ceiling
            let threshold = total_stake / 3 * 2 + if total_stake % 3 > 0 { 1 } else { 0 };
            voted_stake >= threshold
        }
    }
}

pub mod hotstuff;
pub mod hybrid;
pub mod pacemaker;
pub mod simple;
pub mod slashing;
pub mod vrf_pos;

pub use hotstuff::{ConsensusAction, HotStuffConsensus, TimeoutCertificate, TimeoutVote};
pub use hybrid::HybridConsensus;
pub use pacemaker::Pacemaker;
pub use simple::SimpleConsensus;
pub use slashing::SlashingDetector;
pub use vrf_pos::VrfPosConsensus;

#[cfg(test)]
mod proptest_tests;

#[cfg(test)]
mod finality_tests {
    use super::*;

    #[test]
    fn instant_finality_monotonically_advances() {
        let mut f = InstantFinality::new();
        assert_eq!(f.finalized_slot(), 0);

        assert!(f.check_finality(1));
        assert_eq!(f.finalized_slot(), 1);

        assert!(f.check_finality(5));
        assert_eq!(f.finalized_slot(), 5);

        // Slot <= finalized is not re-reported
        assert!(!f.check_finality(3));
        assert!(!f.check_finality(5));
        assert_eq!(f.finalized_slot(), 5);
    }

    #[test]
    fn instant_finality_default() {
        let f = InstantFinality::default();
        assert_eq!(f.finalized_slot(), 0);
    }

    #[test]
    fn instant_finality_record_block_is_noop() {
        let mut f = InstantFinality::new();
        // Should not panic — default no-op
        f.record_block(H256::zero(), H256::zero(), 1);
        assert_eq!(f.finalized_slot(), 0);
    }
}
