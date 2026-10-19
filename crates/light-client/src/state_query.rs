use aether_state_merkle::MerkleProof;
use aether_types::{Address, H256};
use anyhow::{bail, Result};

/// A state proof received from a full node.
#[derive(Debug, Clone)]
pub struct StateProof {
    /// The Merkle proof for this key.
    pub proof: MerkleProof,
    /// The raw value bytes (if the key exists).
    pub value: Option<Vec<u8>>,
}

/// Light client state query engine.
///
/// Verifies Merkle proofs against the finalized state root
/// to read account balances, UTXO status, etc. without
/// downloading the full state.
pub struct StateQuery {
    /// The trusted finalized state root.
    trusted_root: H256,
}

impl StateQuery {
    pub fn new(trusted_root: H256) -> Self {
        StateQuery { trusted_root }
    }

    /// Update the trusted root (after verifying a new finalized header).
    pub fn update_root(&mut self, new_root: H256) {
        self.trusted_root = new_root;
    }

    /// Verify a state proof for an account.
    ///
    /// Returns the value if the proof is valid, or an error if verification fails.
    pub fn verify_account(&self, address: &Address, proof: &StateProof) -> Result<Option<Vec<u8>>> {
        // Check the proof's claimed root matches our trusted root
        if proof.proof.root != self.trusted_root {
            bail!(
                "proof root {:?} does not match trusted root {:?}",
                proof.proof.root,
                self.trusted_root
            );
        }

        // Verify the Merkle proof
        if !proof.proof.verify() {
            bail!("Merkle proof verification failed for {:?}", address);
        }

        // Check the proof is for the right key
        if proof.proof.key != *address {
            bail!("proof key mismatch");
        }

        Ok(proof.value.clone())
    }

    /// Verify an inclusion proof (key exists with a specific value hash).
    pub fn verify_inclusion(
        &self,
        address: &Address,
        expected_value_hash: &H256,
        proof: &StateProof,
    ) -> Result<bool> {
        self.verify_account(address, proof)?;

        match &proof.proof.value_hash {
            Some(vh) => Ok(vh == expected_value_hash),
            None => Ok(false), // Key doesn't exist
        }
    }

    /// Verify an exclusion proof (key does NOT exist).
    pub fn verify_exclusion(&self, address: &Address, proof: &StateProof) -> Result<bool> {
        self.verify_account(address, proof)?;
        Ok(proof.proof.value_hash.is_none())
    }

    pub fn trusted_root(&self) -> H256 {
        self.trusted_root
    }
}
