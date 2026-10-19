use aether_types::{Address, Transaction, H256};
use anyhow::{bail, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// A commitment to a transaction (submitted during the commit phase).
///
/// Contains only the hash of the encrypted transaction, so the proposer
/// cannot see or reorder based on content.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TransactionCommitment {
    /// SHA-256(encrypted_tx_bytes || salt)
    pub commitment_hash: H256,
    /// Who submitted the commitment.
    pub sender: Address,
    /// Slot in which this commitment was included.
    pub commit_slot: u64,
    /// Fee prepaid for commitment inclusion.
    pub commit_fee: u128,
}

/// A revealed transaction that matches a prior commitment.
#[derive(Debug, Clone)]
pub struct RevealedTransaction {
    /// The actual transaction.
    pub transaction: Transaction,
    /// The salt used in the commitment hash.
    pub salt: [u8; 32],
    /// The commitment this reveal matches.
    pub commitment_hash: H256,
}

/// Manages the commit-reveal lifecycle for MEV-resistant ordering.
pub struct CommitRevealPool {
    /// Pending commitments: commitment_hash → commitment
    commitments: HashMap<H256, TransactionCommitment>,
    /// Revealed transactions: commitment_hash → revealed tx
    reveals: HashMap<H256, RevealedTransaction>,
    /// How many slots after commit before reveal is accepted.
    reveal_delay: u64,
    /// How many slots after commit before commitment expires.
    commitment_ttl: u64,
}

impl CommitRevealPool {
    pub fn new(reveal_delay: u64, commitment_ttl: u64) -> Self {
        CommitRevealPool {
            commitments: HashMap::new(),
            reveals: HashMap::new(),
            reveal_delay,
            commitment_ttl,
        }
    }

    /// Create a commitment hash for a transaction.
    ///
    /// commitment_hash = SHA-256(serialize(tx) || salt)
    pub fn create_commitment(tx: &Transaction, salt: &[u8; 32]) -> Result<H256> {
        let tx_bytes = bincode::serialize(tx)
            .map_err(|e| anyhow::anyhow!("transaction serialization failed: {}", e))?;
        let mut hasher = Sha256::new();
        hasher.update(&tx_bytes);
        hasher.update(salt);
        Ok(H256::from_slice(&hasher.finalize()).expect("SHA256 always produces 32 bytes"))
    }

    /// Submit a commitment (commit phase).
    pub fn submit_commitment(&mut self, commitment: TransactionCommitment) -> Result<()> {
        if self.commitments.contains_key(&commitment.commitment_hash) {
            bail!("duplicate commitment");
        }
        self.commitments
            .insert(commitment.commitment_hash, commitment);
        Ok(())
    }

    /// Reveal a transaction that matches a prior commitment (reveal phase).
    pub fn reveal(&mut self, tx: Transaction, salt: [u8; 32], current_slot: u64) -> Result<()> {
        // Compute what the commitment hash should be
        let expected_hash = Self::create_commitment(&tx, &salt)?;

        // Find the matching commitment
        let commitment = self
            .commitments
            .get(&expected_hash)
            .ok_or_else(|| anyhow::anyhow!("no matching commitment found"))?;

        // Check reveal timing: must be after reveal_delay
        if current_slot < commitment.commit_slot + self.reveal_delay {
            bail!(
                "reveal too early: current slot {} < commit slot {} + delay {}",
                current_slot,
                commitment.commit_slot,
                self.reveal_delay
            );
        }

        // Check commitment hasn't expired
        if current_slot > commitment.commit_slot + self.commitment_ttl {
            bail!("commitment expired");
        }

        // Check sender matches
        if tx.sender != commitment.sender {
            bail!("reveal sender does not match commitment sender");
        }

        // Store the revealed transaction
        self.reveals.insert(
            expected_hash,
            RevealedTransaction {
                transaction: tx,
                salt,
                commitment_hash: expected_hash,
            },
        );

        Ok(())
    }

    /// Get all revealed transactions ready for execution.
    /// Returns them in deterministic order: sorted by commitment slot,
    /// then by commitment hash for tie-breaking. This ensures all validators
    /// produce identical transaction ordering (HashMap iteration is undefined).
    pub fn get_revealed_transactions(&self) -> Vec<&RevealedTransaction> {
        let mut revealed: Vec<&RevealedTransaction> = self.reveals.values().collect();
        revealed.sort_by(|a, b| {
            let slot_a = self
                .commitments
                .get(&a.commitment_hash)
                .map_or(0, |c| c.commit_slot);
            let slot_b = self
                .commitments
                .get(&b.commitment_hash)
                .map_or(0, |c| c.commit_slot);
            slot_a
                .cmp(&slot_b)
                .then_with(|| a.commitment_hash.0.cmp(&b.commitment_hash.0))
        });
        revealed
    }

    /// Clean up expired commitments.
    pub fn cleanup_expired(&mut self, current_slot: u64) {
        self.commitments
            .retain(|_, c| current_slot <= c.commit_slot + self.commitment_ttl);
    }

    /// Remove a commitment + reveal after execution.
    pub fn remove(&mut self, commitment_hash: &H256) {
        self.commitments.remove(commitment_hash);
        self.reveals.remove(commitment_hash);
    }

    pub fn pending_commitments(&self) -> usize {
        self.commitments.len()
    }

    pub fn pending_reveals(&self) -> usize {
        self.reveals.len()
    }
}
