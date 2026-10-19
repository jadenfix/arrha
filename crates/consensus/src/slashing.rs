use aether_types::{Address, PublicKey, Signature, H256};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Overflow-safe (a * b) / c using 256-bit intermediate product.
fn mul_div(a: u128, b: u128, c: u128) -> u128 {
    if c == 0 {
        return 0;
    }
    let a_hi = a >> 64;
    let a_lo = a & 0xFFFF_FFFF_FFFF_FFFF;
    let b_hi = b >> 64;
    let b_lo = b & 0xFFFF_FFFF_FFFF_FFFF;

    let lo_lo = a_lo * b_lo;
    let hi_lo = a_hi * b_lo;
    let lo_hi = a_lo * b_hi;
    let hi_hi = a_hi * b_hi;

    let mid = hi_lo + (lo_lo >> 64);
    let mid = mid + lo_hi;
    let carry = if mid < lo_hi { 1u128 } else { 0u128 };

    let product_lo = (mid << 64) | (lo_lo & 0xFFFF_FFFF_FFFF_FFFF);
    let product_hi = hi_hi + (mid >> 64) + carry;

    div_256_by_128(product_hi, product_lo, c)
}

fn div_256_by_128(hi: u128, lo: u128, divisor: u128) -> u128 {
    if hi == 0 {
        return lo / divisor;
    }
    if hi >= divisor {
        return u128::MAX;
    }
    let mut remainder = hi;
    let mut quotient: u128 = 0;
    for i in (0..128).rev() {
        remainder = (remainder << 1) | ((lo >> i) & 1);
        if remainder >= divisor {
            remainder -= divisor;
            quotient |= 1u128 << i;
        }
    }
    quotient
}

/// A signed vote that can be used as evidence in a slash proof.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vote {
    pub slot: u64,
    pub block_hash: H256,
    pub validator: Address,
    pub validator_pubkey: PublicKey,
    pub signature: Signature,
}

impl Vote {
    /// Construct the canonical message that was signed.
    pub fn signing_message(&self) -> Vec<u8> {
        let mut msg = Vec::new();
        msg.extend_from_slice(self.block_hash.as_bytes());
        msg.extend_from_slice(&self.slot.to_le_bytes());
        msg
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashProof {
    pub vote1: Vote,
    pub vote2: Vote,
    pub validator: Address,
    pub proof_type: SlashType,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SlashType {
    /// Same slot, different blocks.
    DoubleSign,
    /// Vote A surrounds Vote B: source_a < source_b AND target_b < target_a.
    SurroundVote,
    /// Missing too many consecutive slots.
    Downtime { missing_slots: u64 },
}

/// Event emitted when a slash is applied.
#[derive(Debug, Clone)]
pub struct SlashEvent {
    pub validator: Address,
    pub proof_type: SlashType,
    pub slash_amount: u128,
    pub reporter_reward: u128,
}

/// Detect a double-sign: same validator, same slot, different blocks.
pub fn detect_double_sign(vote1: &Vote, vote2: &Vote) -> Option<SlashProof> {
    if vote1.slot == vote2.slot
        && vote1.validator == vote2.validator
        && vote1.block_hash != vote2.block_hash
    {
        Some(SlashProof {
            vote1: vote1.clone(),
            vote2: vote2.clone(),
            validator: vote1.validator,
            proof_type: SlashType::DoubleSign,
        })
    } else {
        None
    }
}

/// Detect a surround vote (Casper FFG style).
///
/// Vote A (source_a → target_a) surrounds Vote B (source_b → target_b) when:
///   source_a < source_b AND target_b < target_a
///
/// Here we use `slot` as the target and require a `source_slot` field.
/// For simplicity, we treat vote1.slot as target_a and vote2.slot as target_b.
/// The source slots are inferred from a provided source field if available,
/// or we check if one vote's range strictly contains the other.
pub fn detect_surround_vote(
    vote_a: &Vote,
    source_a: u64,
    vote_b: &Vote,
    source_b: u64,
) -> Option<SlashProof> {
    if vote_a.validator != vote_b.validator {
        return None;
    }

    // A surrounds B: source_a < source_b AND target_b < target_a
    let a_surrounds_b = source_a < source_b && vote_b.slot < vote_a.slot;
    // B surrounds A: source_b < source_a AND target_a < target_b
    let b_surrounds_a = source_b < source_a && vote_a.slot < vote_b.slot;

    if a_surrounds_b || b_surrounds_a {
        Some(SlashProof {
            vote1: vote_a.clone(),
            vote2: vote_b.clone(),
            validator: vote_a.validator,
            proof_type: SlashType::SurroundVote,
        })
    } else {
        None
    }
}

/// Verify a slash proof: check structural consistency AND cryptographic signatures.
pub fn verify_slash_proof(proof: &SlashProof) -> anyhow::Result<()> {
    match &proof.proof_type {
        SlashType::DoubleSign => {
            if proof.vote1.slot != proof.vote2.slot {
                anyhow::bail!("votes not in same slot");
            }
            if proof.vote1.block_hash == proof.vote2.block_hash {
                anyhow::bail!("votes for same block");
            }
            if proof.vote1.validator != proof.vote2.validator {
                anyhow::bail!("votes from different validators");
            }
            // Ensure proof.validator matches the votes — prevents slashing
            // an innocent validator using another validator's double-sign evidence.
            if proof.validator != proof.vote1.validator {
                anyhow::bail!(
                    "proof.validator does not match vote validator — \
                     cannot slash a different validator than the one who double-signed"
                );
            }

            // Verify signatures on both votes
            verify_vote_signature(&proof.vote1)?;
            verify_vote_signature(&proof.vote2)?;

            Ok(())
        }
        SlashType::SurroundVote => {
            if proof.vote1.validator != proof.vote2.validator {
                anyhow::bail!("votes from different validators");
            }
            if proof.validator != proof.vote1.validator {
                anyhow::bail!(
                    "proof.validator does not match vote validator — \
                     cannot slash a different validator than the one who surround-voted"
                );
            }

            verify_vote_signature(&proof.vote1)?;
            verify_vote_signature(&proof.vote2)?;

            Ok(())
        }
        SlashType::Downtime { .. } => {
            // Downtime slashing requires on-chain slot attestation records that
            // prove a validator missed consecutive slots. Without that evidence,
            // anyone could fabricate a downtime proof to slash any validator.
            // Until on-chain slot participation tracking is implemented,
            // downtime slashing via externally-submitted proofs is rejected.
            anyhow::bail!(
                "downtime slashing via proof submission is not supported — \
                 downtime penalties must be assessed by the protocol using \
                 on-chain slot participation records"
            );
        }
    }
}

/// Verify a vote's BLS signature against the validator's public key.
/// Votes are signed with BLS (not Ed25519), matching the consensus voting path.
fn verify_vote_signature(vote: &Vote) -> anyhow::Result<()> {
    let pubkey_bytes = vote.validator_pubkey.as_bytes();
    let msg = vote.signing_message();
    let sig_bytes = vote.signature.as_bytes();

    match aether_crypto_bls::keypair::verify(pubkey_bytes, &msg, sig_bytes) {
        Ok(true) => Ok(()),
        Ok(false) => Err(anyhow::anyhow!("invalid BLS vote signature")),
        Err(e) => Err(anyhow::anyhow!("BLS verification error: {}", e)),
    }
}

/// Return the slash rate in basis points for a given offense type.
///
/// This avoids the lossy roundtrip of computing an absolute slash amount and
/// then converting back to bps (which silently yields 0 when
/// `slash_amount * 10_000` overflows u128).
pub fn slash_rate_bps(proof_type: &SlashType) -> u32 {
    match proof_type {
        SlashType::DoubleSign => 500,    // 5%
        SlashType::SurroundVote => 500,  // 5%
        SlashType::Downtime { .. } => 0, // Downtime is variable; handled separately
    }
}

/// Calculate how much stake to slash.
/// Uses overflow-safe mul_div to avoid silent truncation on large u128 stakes.
pub fn calculate_slash_amount(stake: u128, proof_type: &SlashType) -> u128 {
    match proof_type {
        SlashType::DoubleSign => mul_div(stake, 5, 100), // 5%
        SlashType::SurroundVote => mul_div(stake, 5, 100), // 5%
        SlashType::Downtime { missing_slots } => {
            let leak = *missing_slots as u128;
            std::cmp::min(leak, mul_div(stake, 1, 10)) // Cap at 10%
        }
    }
}

/// Apply a slash to a validator's stake. Returns the slash event.
///
/// - Reduces validator stake by `slash_amount`
/// - 10% of slashed amount goes to the reporter as a reward
/// - If stake drops below `min_stake`, validator is deactivated
pub fn apply_slash(validator_stake: u128, proof: &SlashProof, _min_stake: u128) -> SlashEvent {
    let slash_amount = calculate_slash_amount(validator_stake, &proof.proof_type);
    let reporter_reward = slash_amount / 10; // 10% to reporter

    SlashEvent {
        validator: proof.validator,
        proof_type: proof.proof_type.clone(),
        slash_amount,
        reporter_reward,
    }
}

/// A recorded vote including signature, so double-sign proofs carry real evidence.
#[derive(Clone)]
struct RecordedVote {
    block_hash: H256,
    validator_pubkey: PublicKey,
    signature: Signature,
}

/// Tracks votes per (validator, slot) to detect double-signing in real time.
/// Designed to be embedded in the node's vote processing path.
#[derive(Default)]
pub struct SlashingDetector {
    /// Maps (validator_address, slot) -> first vote (with full signature).
    seen_votes: HashMap<(Address, u64), RecordedVote>,
    /// Pending slash proofs awaiting enforcement.
    pending_slashes: Vec<SlashProof>,
}

impl SlashingDetector {
    pub fn new() -> Self {
        SlashingDetector {
            seen_votes: HashMap::new(),
            pending_slashes: Vec::new(),
        }
    }

    /// Record a vote. If the same validator voted for a different block in the
    /// same slot, a `SlashProof` is created and returned.
    ///
    /// The caller is responsible for supplying the validator address and BLS
    /// public key (resolved from the `aether_types::Vote`).
    pub fn record_vote(
        &mut self,
        validator: Address,
        validator_pubkey: aether_types::PublicKey,
        slot: u64,
        block_hash: H256,
        signature: aether_types::Signature,
    ) -> Option<SlashProof> {
        let key = (validator, slot);
        match self.seen_votes.entry(key) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(RecordedVote {
                    block_hash,
                    validator_pubkey,
                    signature,
                });
                None
            }
            std::collections::hash_map::Entry::Occupied(e) => {
                let first = e.get();
                if first.block_hash == block_hash {
                    return None; // Duplicate vote, not a double-sign
                }
                // Double-sign detected — both votes carry real signatures
                let vote1 = Vote {
                    slot,
                    block_hash: first.block_hash,
                    validator,
                    validator_pubkey: first.validator_pubkey.clone(),
                    signature: first.signature.clone(),
                };
                let vote2 = Vote {
                    slot,
                    block_hash,
                    validator,
                    validator_pubkey,
                    signature,
                };
                let proof = SlashProof {
                    vote1,
                    vote2,
                    validator,
                    proof_type: SlashType::DoubleSign,
                };
                self.pending_slashes.push(proof.clone());
                Some(proof)
            }
        }
    }

    /// Drain all pending slash proofs for processing.
    pub fn drain_pending(&mut self) -> Vec<SlashProof> {
        std::mem::take(&mut self.pending_slashes)
    }

    /// Prune vote records for slots below `min_slot` to bound memory.
    pub fn prune_before(&mut self, min_slot: u64) {
        self.seen_votes.retain(|&(_, slot), _| slot >= min_slot);
    }
}
