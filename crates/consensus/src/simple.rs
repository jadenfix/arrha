// Simplified consensus for initial implementation
// Full VRF-PoS + HotStuff will be added progressively

use crate::{ConsensusEngine, Finality};
use aether_types::{Block, PublicKey, Slot, ValidatorInfo, Vote};
use anyhow::{bail, Result};
use std::collections::HashMap;

pub struct SimpleConsensus {
    validators: Vec<ValidatorInfo>,
    current_slot: Slot,
    finalized_slot: Slot,
    votes: HashMap<Slot, Vec<Vote>>,
}

impl SimpleConsensus {
    pub fn new(validators: Vec<ValidatorInfo>) -> Self {
        SimpleConsensus {
            validators,
            current_slot: 0,
            finalized_slot: 0,
            votes: HashMap::new(),
        }
    }

    pub fn current_slot(&self) -> Slot {
        self.current_slot
    }

    pub fn advance_slot(&mut self) {
        self.current_slot = self.current_slot.saturating_add(1);
    }

    // Simplified leader election - round-robin by stake
    pub fn get_leader(&self, slot: Slot) -> Option<&ValidatorInfo> {
        if self.validators.is_empty() {
            return None;
        }

        let index = (slot as usize) % self.validators.len();
        Some(&self.validators[index])
    }

    pub fn is_leader(&self, slot: Slot, validator_pubkey: &PublicKey) -> bool {
        if let Some(leader) = self.get_leader(slot) {
            &leader.pubkey == validator_pubkey
        } else {
            false
        }
    }

    pub fn validate_block(&self, block: &Block) -> Result<()> {
        // Check slot is current or recent
        if block.header.slot > self.current_slot {
            bail!("block from future slot");
        }

        // Check proposer is valid leader
        let leader = self
            .get_leader(block.header.slot)
            .ok_or_else(|| anyhow::anyhow!("no leader for slot"))?;

        if block.header.proposer != leader.pubkey.to_address() {
            bail!("invalid proposer");
        }

        Ok(())
    }

    pub fn add_vote(&mut self, vote: Vote) -> Result<()> {
        // Validate vote slot
        if vote.slot > self.current_slot {
            bail!("vote from future slot");
        }

        // Add to votes
        self.votes.entry(vote.slot).or_default().push(vote);

        Ok(())
    }

    pub fn check_finality(&mut self, slot: Slot) -> bool {
        let votes = match self.votes.get(&slot) {
            Some(v) => v,
            None => return false,
        };

        // Calculate total voting stake
        let total_stake: u128 = self
            .validators
            .iter()
            .map(|v| v.stake)
            .fold(0u128, u128::saturating_add);
        let mut voted_stake = 0u128;

        for vote in votes {
            voted_stake = voted_stake.saturating_add(vote.stake);
        }

        // Check if ≥2/3 stake voted
        let quorum_reached = voted_stake.saturating_mul(3) >= total_stake.saturating_mul(2);

        if quorum_reached && slot > self.finalized_slot {
            self.finalized_slot = slot;
            true
        } else {
            false
        }
    }

    pub fn finalized_slot(&self) -> Slot {
        self.finalized_slot
    }

    pub fn total_stake(&self) -> u128 {
        self.validators
            .iter()
            .map(|v| v.stake)
            .fold(0u128, u128::saturating_add)
    }
}

impl Finality for SimpleConsensus {
    fn check_finality(&mut self, slot: Slot) -> bool {
        SimpleConsensus::check_finality(self, slot)
    }

    fn finalized_slot(&self) -> Slot {
        SimpleConsensus::finalized_slot(self)
    }
}

impl ConsensusEngine for SimpleConsensus {
    fn current_slot(&self) -> Slot {
        self.current_slot
    }

    fn finalizes_on_execution(&self) -> bool {
        true
    }

    fn advance_slot(&mut self) {
        SimpleConsensus::advance_slot(self)
    }

    fn is_leader(&self, slot: Slot, validator_pubkey: &PublicKey) -> bool {
        SimpleConsensus::is_leader(self, slot, validator_pubkey)
    }

    fn validate_block(&self, block: &Block) -> Result<()> {
        SimpleConsensus::validate_block(self, block)
    }

    fn add_vote(&mut self, vote: Vote) -> Result<()> {
        SimpleConsensus::add_vote(self, vote)
    }

    fn total_stake(&self) -> u128 {
        SimpleConsensus::total_stake(self)
    }

    fn validator_addresses_and_stakes(&self) -> Vec<(aether_types::Address, u128)> {
        self.validators
            .iter()
            .filter(|v| v.stake > 0)
            .map(|v| (v.pubkey.to_address(), v.stake))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_crypto_primitives::Keypair;
    use aether_types::{Signature, H256};

    fn create_test_validators(count: usize) -> Vec<ValidatorInfo> {
        (0..count)
            .map(|i| {
                let keypair = Keypair::generate();
                ValidatorInfo {
                    pubkey: PublicKey::from_bytes(keypair.public_key()),
                    stake: 1000 * (i as u128 + 1),
                    commission: 1000, // 10%
                    active: true,
                }
            })
            .collect()
    }

    #[test]
    fn test_leader_election() {
        let validators = create_test_validators(4);
        let consensus = SimpleConsensus::new(validators.clone());

        let leader0 = consensus.get_leader(0).unwrap();
        let leader4 = consensus.get_leader(4).unwrap();

        // Round-robin: slot 4 should have same leader as slot 0
        assert_eq!(leader0.pubkey, leader4.pubkey);
    }

    #[test]
    fn test_finality() {
        let validators = create_test_validators(3);
        let mut consensus = SimpleConsensus::new(validators.clone());

        let slot = 1;
        // Advance to the target slot
        consensus.advance_slot();
        assert_eq!(consensus.current_slot(), slot);

        // Add votes from 2 of 3 validators (2/3 stake)
        for validator in validators.iter().skip(1) {
            let vote = Vote {
                slot,
                block_hash: H256::zero(),
                validator: validator.pubkey.clone(),
                signature: Signature::from_bytes(vec![]),
                stake: validator.stake,
            };
            consensus.add_vote(vote).unwrap();
        }

        assert!(consensus.check_finality(slot));
        assert_eq!(consensus.finalized_slot(), slot);
    }
}
