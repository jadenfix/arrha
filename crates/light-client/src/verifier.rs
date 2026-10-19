use aether_types::{BlockHeader, PublicKey, Slot, H256};
use anyhow::{bail, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Validator info for light client verification.
#[derive(Debug, Clone)]
pub struct ValidatorEntry {
    pub pubkey: PublicKey,
    pub stake: u128,
}

/// Finalized header with aggregate signature proof.
#[derive(Debug, Clone)]
pub struct FinalizedHeader {
    pub header: BlockHeader,
    pub aggregate_signature: Vec<u8>,
    pub signer_pubkeys: Vec<PublicKey>,
    pub total_signing_stake: u128,
}

/// Check if `voted_stake` represents a 2/3 quorum of `total_stake`.
/// Uses checked arithmetic to avoid overflow.
fn has_quorum(voted_stake: u128, total_stake: u128) -> bool {
    if total_stake == 0 {
        return false;
    }
    match (voted_stake.checked_mul(3), total_stake.checked_mul(2)) {
        (Some(lhs), Some(rhs)) => lhs >= rhs,
        _ => {
            let threshold = total_stake / 3 * 2 + if total_stake % 3 > 0 { 1 } else { 0 };
            voted_stake >= threshold
        }
    }
}

/// Light client verifier — checks finality of headers using BLS signatures.
pub struct LightClientVerifier {
    /// Known validator set (updated on epoch boundaries).
    validators: HashMap<Vec<u8>, ValidatorEntry>,
    /// Total stake across all validators.
    total_stake: u128,
    /// Highest verified finalized slot.
    finalized_slot: Slot,
    /// Finalized state root (for Merkle proof verification).
    finalized_state_root: H256,
}

impl LightClientVerifier {
    /// Create a new light client verifier with the initial validator set.
    pub fn new(validators: Vec<ValidatorEntry>) -> Self {
        let total_stake: u128 = validators
            .iter()
            .fold(0u128, |acc, v| acc.saturating_add(v.stake));
        let validators_map: HashMap<Vec<u8>, ValidatorEntry> = validators
            .into_iter()
            .map(|v| (v.pubkey.as_bytes().to_vec(), v))
            .collect();

        LightClientVerifier {
            validators: validators_map,
            total_stake,
            finalized_slot: 0,
            finalized_state_root: H256::zero(),
        }
    }

    /// Verify and accept a finalized header.
    ///
    /// Checks that:
    /// 1. The aggregate signature is valid over the header hash
    /// 2. The signing stake represents ≥2/3 of total stake
    /// 3. The slot advances (no regression)
    pub fn verify_finalized_header(&mut self, finalized: &FinalizedHeader) -> Result<()> {
        let header = &finalized.header;

        // Slot must advance
        if header.slot <= self.finalized_slot {
            bail!(
                "slot {} does not advance beyond finalized slot {}",
                header.slot,
                self.finalized_slot
            );
        }

        // Check quorum: signing stake must be ≥2/3 of total
        if !has_quorum(finalized.total_signing_stake, self.total_stake) {
            bail!(
                "insufficient signing stake: {} < 2/3 of {}",
                finalized.total_signing_stake,
                self.total_stake
            );
        }

        // Verify all signers are known validators.
        // SECURITY: deduplicate signers to prevent stake inflation attack where
        // an attacker repeats the same pubkey to fake quorum with fewer validators.
        let mut seen_signers = std::collections::HashSet::new();
        let mut verified_stake: u128 = 0;
        for pk in &finalized.signer_pubkeys {
            let pk_bytes = pk.as_bytes().to_vec();
            if !seen_signers.insert(pk_bytes) {
                bail!("duplicate signer in finalized header: {:?}", pk);
            }
            match self.validators.get(pk.as_bytes()) {
                Some(entry) => verified_stake = verified_stake.saturating_add(entry.stake),
                None => bail!("unknown signer: {:?}", pk),
            }
        }

        if !has_quorum(verified_stake, self.total_stake) {
            bail!(
                "verified stake {} < 2/3 of total {}",
                verified_stake,
                self.total_stake
            );
        }

        // BLS aggregate signature verification:
        // 1. Aggregate the individual signer public keys
        // 2. Verify the aggregate signature over the header hash
        let signer_pk_bytes: Vec<Vec<u8>> = finalized
            .signer_pubkeys
            .iter()
            .map(|pk| pk.as_bytes().to_vec())
            .collect();

        // Compute the header hash that was signed
        let mut hasher = Sha256::new();
        hasher.update(header.slot.to_le_bytes());
        hasher.update(header.parent_hash.as_bytes());
        hasher.update(header.state_root.as_bytes());
        hasher.update(header.transactions_root.as_bytes());
        hasher.update(header.receipts_root.as_bytes());
        let header_msg = hasher.finalize().to_vec();

        // Aggregate public keys and verify
        if signer_pk_bytes.is_empty() {
            bail!("finalized header at slot {} has no signers", header.slot);
        }
        if !finalized.aggregate_signature.is_empty() {
            let agg_pk = aether_crypto_bls::aggregate_public_keys(&signer_pk_bytes)
                .map_err(|e| anyhow::anyhow!("failed to aggregate signer public keys: {e}"))?;

            let valid = aether_crypto_bls::verify_aggregated(
                &agg_pk,
                &header_msg,
                &finalized.aggregate_signature,
            )
            .map_err(|e| anyhow::anyhow!("BLS verification error: {e}"))?;

            if !valid {
                bail!(
                    "invalid BLS aggregate signature on finalized header at slot {}",
                    header.slot
                );
            }
        } else if finalized.aggregate_signature.is_empty() {
            bail!(
                "finalized header at slot {} has empty aggregate signature",
                header.slot
            );
        }

        // Accept the header
        self.finalized_slot = header.slot;
        self.finalized_state_root = header.state_root;

        Ok(())
    }

    /// Get the current finalized state root (for Merkle proof verification).
    pub fn finalized_state_root(&self) -> H256 {
        self.finalized_state_root
    }

    pub fn finalized_slot(&self) -> Slot {
        self.finalized_slot
    }

    /// Update the validator set (on epoch boundary).
    pub fn update_validators(&mut self, validators: Vec<ValidatorEntry>) {
        self.total_stake = validators
            .iter()
            .fold(0u128, |acc, v| acc.saturating_add(v.stake));
        self.validators = validators
            .into_iter()
            .map(|v| (v.pubkey.as_bytes().to_vec(), v))
            .collect();
    }
}
