use aether_types::{Address, H256};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A batch of L2 transactions posted to L1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L2Batch {
    /// Unique batch identifier.
    pub batch_id: u64,
    /// L2 chain identifier.
    pub chain_id: u64,
    /// Sequencer that created this batch.
    pub sequencer: Address,
    /// L2 state root BEFORE this batch.
    pub pre_state_root: H256,
    /// L2 state root AFTER this batch.
    pub post_state_root: H256,
    /// Hashes of all L2 transactions in this batch.
    pub tx_hashes: Vec<H256>,
    /// L1 slot when this batch was posted.
    pub l1_slot: u64,
}

impl L2Batch {
    /// Hash this batch for commitment.
    pub fn hash(&self) -> H256 {
        let bytes = bincode::serialize(self).expect("L2Batch serialization infallible");
        H256::from(<[u8; 32]>::from(Sha256::digest(&bytes)))
    }
}

/// A state commitment posted on L1 by the sequencer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateCommitment {
    /// The batch this commitment covers.
    pub batch: L2Batch,
    /// Challenge window end slot (after which commitment is finalized).
    pub challenge_deadline: u64,
    /// Whether a fraud proof has been submitted.
    pub challenged: bool,
    /// Whether the commitment is finalized (past challenge window, unchallenged).
    pub finalized: bool,
}

/// Challenge window duration (in L1 slots).
/// 7 days = 7 * 24 * 3600 * 2 = 1,209,600 slots at 500ms.
pub const CHALLENGE_WINDOW_SLOTS: u64 = 1_209_600;

impl StateCommitment {
    pub fn new(batch: L2Batch, current_l1_slot: u64) -> Self {
        StateCommitment {
            challenge_deadline: current_l1_slot.saturating_add(CHALLENGE_WINDOW_SLOTS),
            batch,
            challenged: false,
            finalized: false,
        }
    }

    /// Check if the challenge window has passed.
    pub fn is_past_challenge_window(&self, current_l1_slot: u64) -> bool {
        current_l1_slot > self.challenge_deadline
    }

    /// Finalize this commitment (only if unchallenged and past deadline).
    pub fn try_finalize(&mut self, current_l1_slot: u64) -> Result<(), String> {
        if self.challenged {
            return Err("commitment has been challenged".into());
        }
        if !self.is_past_challenge_window(current_l1_slot) {
            return Err("challenge window not expired".into());
        }
        self.finalized = true;
        Ok(())
    }
}
