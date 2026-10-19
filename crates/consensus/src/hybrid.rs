use aether_types::saturating_mul_div as mul_div;
// ============================================================================
// HYBRID CONSENSUS - Phase 1 Full Integration
// ============================================================================
// Combines VRF-PoS leader election + HotStuff BFT + BLS signature aggregation
// ============================================================================

use crate::{ConsensusEngine, Pacemaker};
use aether_crypto_bls::{aggregate_public_keys, aggregate_signatures, BlsKeypair};
use aether_crypto_vrf::{
    check_leader_eligibility_integer, EcVrfVerifier, VrfKeypair, VrfProof, VrfSigner, VrfVerifier,
};
use aether_types::{Address, AggregatedVote, Block, PublicKey, Slot, ValidatorInfo, Vote, H256};
use anyhow::{bail, Result};
use sha2::{Digest, Sha256};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

/// Votes are accepted for blocks up to this many slots old. Validators run
/// in separate processes, so a vote for slot `s` routinely arrives after the
/// receiver has moved on to `s + 1`.
pub const VOTE_WINDOW_SLOTS: u64 = 64;

/// A certified block: a quorum (>= 2/3 stake) of BLS votes for it.
#[derive(Debug, Clone)]
struct Certified {
    slot: Slot,
    qc: AggregatedVote,
}

/// HotStuff consensus phases
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Phase {
    Propose,
    Prevote,
    Precommit,
    Commit,
}

/// Aggregated vote certificate (QC)
#[derive(Debug, Clone)]
pub struct QuorumCertificate {
    pub slot: Slot,
    pub block_hash: H256,
    pub phase: Phase,
    pub total_stake: u128,
    pub signers: Vec<Address>,
    pub aggregated_signature: Vec<u8>,
    pub aggregated_pubkey: Vec<u8>,
}

/// Full Phase 1 consensus combining:
/// - VRF-PoS for leader election
/// - HotStuff 2-chain for BFT finality
/// - BLS for vote aggregation
pub struct HybridConsensus {
    // === Validator Set ===
    /// Live validator set (updated by slashing/staking mid-epoch).
    validators: HashMap<Address, ValidatorInfo>,
    total_stake: u128,

    /// Epoch-frozen validator snapshot used for leader election.
    /// Fixed genesis committee for protocol v2; stake reconfiguration is only
    /// safe after a finalized on-chain transition is implemented.
    epoch_validators: HashMap<Address, ValidatorInfo>,
    epoch_total_stake: u128,

    // === Slot/Epoch Management ===
    current_slot: Slot,
    current_epoch: u64,
    epoch_randomness: H256,
    /// Seed changes indexed by the first slot that uses each seed. This lets
    /// peers validate delayed blocks with the same historical VRF input.
    randomness_history: BTreeMap<Slot, H256>,
    epoch_length: u64,
    /// Whether epoch randomness has been updated from a real VRF output this epoch.
    epoch_randomness_updated: bool,

    // === VRF-PoS Parameters ===
    tau_numerator: u128, // Integer numerator for deterministic eligibility check
    tau_denominator: u128, // Integer denominator for deterministic eligibility check
    my_vrf_keypair: Option<Box<dyn VrfSigner>>,
    vrf_verifier: Box<dyn VrfVerifier>,
    my_bls_keypair: Option<BlsKeypair>,
    my_address: Option<Address>,

    // === HotStuff State ===
    current_phase: Phase,
    /// Votes deduplicated by validator address: one vote per (slot, phase, block, validator).
    votes: HashMap<(Slot, Phase, H256), HashMap<Address, Vote>>,
    qcs: HashMap<(Slot, Phase, H256), QuorumCertificate>,
    locked_block: Option<H256>,
    locked_slot: Slot,

    // === Block Parent Tracking (for 2-chain finality) ===
    block_parents: HashMap<H256, H256>,
    block_slots: HashMap<H256, Slot>,

    /// Track which block each validator voted for per slot (equivocation detection).
    /// Key: (slot, validator_address) → block_hash they voted for.
    vote_record: HashMap<(Slot, Address), H256>,

    // === VRF Public Keys (for verifying other validators' proofs) ===
    vrf_pubkeys: HashMap<Address, [u8; 32]>,
    /// BLS public keys for vote signature verification
    bls_pubkeys: HashMap<Address, Vec<u8>>,

    // === Pacemaker (timeout-based phase advancement) ===
    pacemaker: Pacemaker,

    // === Certified blocks (QCs formed locally or received in blocks) ===
    certified: HashMap<H256, Certified>,
    high_qc: Option<(H256, Slot)>,

    // === Finality ===
    committed_slot: Slot,
    finalized_slot: Slot,
    finalized_block: Option<H256>,
    last_reported_finalized: Slot,
}

impl HybridConsensus {
    pub fn new(
        validators: Vec<ValidatorInfo>,
        tau: f64,
        epoch_length: u64,
        my_vrf_keypair: Option<VrfKeypair>,
        my_bls_keypair: Option<BlsKeypair>,
        my_address: Option<Address>,
    ) -> Self {
        Self::with_vrf(
            validators,
            tau,
            epoch_length,
            my_vrf_keypair.map(|k| Box::new(k) as Box<dyn VrfSigner>),
            Box::new(EcVrfVerifier),
            my_bls_keypair,
            my_address,
        )
    }

    pub fn with_vrf(
        validators: Vec<ValidatorInfo>,
        tau: f64,
        epoch_length: u64,
        my_vrf_keypair: Option<Box<dyn VrfSigner>>,
        vrf_verifier: Box<dyn VrfVerifier>,
        my_bls_keypair: Option<BlsKeypair>,
        my_address: Option<Address>,
    ) -> Self {
        // Guard against division-by-zero in advance_slot epoch boundary check.
        let epoch_length = epoch_length.max(1);
        let total_stake: u128 = validators
            .iter()
            .map(|v| v.stake)
            .fold(0u128, u128::saturating_add);
        let validators_map: HashMap<Address, ValidatorInfo> = validators
            .into_iter()
            .map(|v| (v.pubkey.to_address(), v))
            .collect();

        // Convert f64 tau to integer fraction: multiply by 10000 to preserve 4 decimal places
        let tau_clamped = if tau.is_finite() {
            tau.clamp(0.0, 1.0)
        } else {
            0.5
        };
        let tau_numerator = (tau_clamped * 10000.0).round() as u128;
        let tau_denominator = 10000u128;

        HybridConsensus {
            epoch_validators: validators_map.clone(),
            epoch_total_stake: total_stake,
            validators: validators_map,
            total_stake,
            current_slot: 0,
            current_epoch: 0,
            epoch_randomness: H256::zero(),
            randomness_history: BTreeMap::from([(0, H256::zero())]),
            epoch_length,
            epoch_randomness_updated: false,
            tau_numerator,
            tau_denominator,
            my_vrf_keypair,
            vrf_verifier,
            my_bls_keypair,
            my_address,
            current_phase: Phase::Propose,
            votes: HashMap::new(),
            qcs: HashMap::new(),
            locked_block: None,
            locked_slot: 0,
            block_parents: HashMap::new(),
            block_slots: HashMap::new(),
            vote_record: HashMap::new(),
            vrf_pubkeys: HashMap::new(),
            bls_pubkeys: HashMap::new(),
            pacemaker: Pacemaker::new(Duration::from_millis(500)),
            certified: HashMap::new(),
            high_qc: None,
            committed_slot: 0,
            finalized_slot: 0,
            finalized_block: None,
            last_reported_finalized: 0,
        }
    }

    /// Record that `block_hash` (at `slot`) is certified, update the lock and
    /// high QC, and apply the 2-chain commit rule in both directions.
    fn record_certified(&mut self, block_hash: H256, slot: Slot, qc: AggregatedVote) {
        if self.certified.contains_key(&block_hash) {
            return;
        }
        self.certified.insert(block_hash, Certified { slot, qc });
        if self.high_qc.is_none_or(|(_, s)| slot > s) {
            self.high_qc = Some((block_hash, slot));
        }
        // Lock on the highest certified block: we will only vote for blocks
        // whose (certified) parent is at least this high.
        if slot >= self.locked_slot {
            self.locked_block = Some(block_hash);
            self.locked_slot = slot;
        }
        if slot > self.committed_slot {
            self.committed_slot = slot;
        }
        // This block may be the certified child of a certified parent...
        self.try_commit_parent_of(block_hash);
        // ...or the certified parent of an already-certified child.
        let children: Vec<H256> = self
            .block_parents
            .iter()
            .filter(|(_, parent)| **parent == block_hash)
            .map(|(child, _)| *child)
            .collect();
        for child in children {
            self.try_commit_parent_of(child);
        }
    }

    /// 2-chain commit rule: if `child` and its parent are both certified and
    /// the child is in the slot directly after the parent, the parent (and
    /// all its ancestors) are final. Requiring consecutive slots is what
    /// makes the 2-chain rule safe (as in DiemBFT v4 / Jolteon); a gap would
    /// let a conflicting certified block exist in between.
    fn try_commit_parent_of(&mut self, child: H256) {
        let child_slot = match self.certified.get(&child) {
            Some(c) => c.slot,
            None => return,
        };
        let parent = match self.block_parents.get(&child) {
            Some(p) => *p,
            None => return,
        };
        let parent_slot = match self.certified.get(&parent) {
            Some(c) => c.slot,
            None => return,
        };
        if parent_slot.saturating_add(1) == child_slot && parent_slot > self.finalized_slot {
            self.finalized_slot = parent_slot;
            self.finalized_block = Some(parent);
            tracing::info!(
                finalized_slot = parent_slot,
                child_slot,
                "2-chain finality: block finalized via consecutive QCs"
            );
        }
    }

    /// Build the wire-format QC for a set of votes on one block.
    fn wire_qc(&self, qc: &QuorumCertificate) -> AggregatedVote {
        let signers = qc
            .signers
            .iter()
            .filter_map(|addr| self.validators.get(addr).map(|v| v.pubkey.clone()))
            .collect();
        AggregatedVote {
            slot: qc.slot,
            block_hash: qc.block_hash,
            aggregated_signature: qc.aggregated_signature.clone(),
            signers,
            total_stake: qc.total_stake,
        }
    }

    /// Check if I am eligible to be leader for this slot
    pub fn check_my_eligibility(&self, slot: Slot) -> Option<VrfProof> {
        let vrf_keypair = self.my_vrf_keypair.as_ref()?;
        let my_addr = self.my_address.as_ref()?;
        // Use epoch-frozen validator set for deterministic leader election.
        let validator = self.epoch_validators.get(my_addr)?;

        // Compute VRF input: epoch_randomness || slot
        let mut input = Vec::new();
        let randomness = self.randomness_for_slot(slot);
        input.extend_from_slice(randomness.as_bytes());
        input.extend_from_slice(&slot.to_le_bytes());

        let proof = VrfSigner::prove(vrf_keypair.as_ref(), &input);

        // Check eligibility threshold against epoch-frozen stake
        if check_leader_eligibility_integer(
            &proof.output,
            validator.stake,
            self.epoch_total_stake,
            self.tau_numerator,
            self.tau_denominator,
        ) {
            Some(proof)
        } else {
            None
        }
    }

    /// Register a validator's VRF public key for cross-validation.
    pub fn register_vrf_pubkey(&mut self, address: Address, vrf_pubkey: [u8; 32]) {
        self.vrf_pubkeys.insert(address, vrf_pubkey);
    }

    /// Verify that a block's proposer was eligible
    pub fn verify_leader_eligibility(&self, block: &Block) -> Result<bool> {
        let proposer_addr = block.header.proposer;
        // Use epoch-frozen validator set for verification consistency.
        let validator = self
            .epoch_validators
            .get(&proposer_addr)
            .ok_or_else(|| anyhow::anyhow!("unknown validator"))?;

        // Reconstruct VRF input
        let mut input = Vec::new();
        let randomness = self.randomness_for_slot(block.header.slot);
        input.extend_from_slice(randomness.as_bytes());
        input.extend_from_slice(&block.header.slot.to_le_bytes());

        // Convert VRF proof
        let vrf_proof = VrfProof {
            output: block.header.vrf_proof.output,
            proof: block.header.vrf_proof.proof.clone(),
        };

        let vrf_pubkey: [u8; 32] = *self.vrf_pubkeys.get(&proposer_addr).ok_or_else(|| {
            anyhow::anyhow!(
                "no VRF public key registered for proposer {:?}",
                proposer_addr
            )
        })?;

        if !self.vrf_verifier.verify(&vrf_pubkey, &input, &vrf_proof)? {
            return Ok(false);
        }

        Ok(check_leader_eligibility_integer(
            &vrf_proof.output,
            validator.stake,
            self.epoch_total_stake,
            self.tau_numerator,
            self.tau_denominator,
        ))
    }

    /// Create a vote for a block (BLS signature)
    pub fn create_vote(&self, block_hash: H256, _phase: Phase) -> Result<Option<Vote>> {
        let bls_keypair = match &self.my_bls_keypair {
            Some(kp) => kp,
            None => return Ok(None), // Not a validator
        };

        let my_addr = match &self.my_address {
            Some(addr) => addr,
            None => return Ok(None),
        };

        let validator = self
            .epoch_validators
            .get(my_addr)
            .ok_or_else(|| anyhow::anyhow!("not in validator set"))?;

        // Create vote message: block_hash || slot (standardized format)
        let mut msg = Vec::new();
        msg.extend_from_slice(block_hash.as_bytes());
        msg.extend_from_slice(&self.current_slot.to_le_bytes());

        // Sign with BLS
        let signature = bls_keypair.sign(&msg);

        Ok(Some(Vote {
            slot: self.current_slot,
            block_hash,
            validator: validator.pubkey.clone(),
            signature: aether_types::Signature::from_bytes(signature),
            stake: validator.stake,
        }))
    }

    /// Register a validator's BLS public key for vote signature verification.
    ///
    /// Requires a valid proof-of-possession (PoP) signature to prevent rogue key attacks.
    /// The PoP proves the registrant knows the secret key corresponding to the public key.
    pub fn register_bls_pubkey(
        &mut self,
        address: Address,
        bls_pubkey: Vec<u8>,
        pop_signature: &[u8],
    ) -> Result<()> {
        if bls_pubkey.len() != 48 {
            bail!("BLS pubkey must be 48 bytes, got {}", bls_pubkey.len());
        }
        // Verify proof-of-possession to prevent rogue key attacks
        match aether_crypto_bls::verify_pop(&bls_pubkey, pop_signature)? {
            true => {}
            false => bail!(
                "invalid proof-of-possession for BLS pubkey registered by {:?}",
                address
            ),
        }
        self.bls_pubkeys.insert(address, bls_pubkey);
        Ok(())
    }

    /// Update epoch randomness using a real VRF output from the first finalized block.
    /// Returns true if this was the first update this epoch (idempotent guard).
    pub fn update_epoch_randomness(&mut self, block_vrf_output: &[u8; 32]) -> bool {
        if self.epoch_randomness_updated {
            return false;
        }
        let mut hasher = Sha256::new();
        hasher.update(self.epoch_randomness.as_bytes());
        hasher.update(block_vrf_output);
        hasher.update(self.current_epoch.to_le_bytes());
        self.epoch_randomness = H256::from(<[u8; 32]>::from(hasher.finalize()));
        self.epoch_randomness_updated = true;
        self.randomness_history
            .insert(self.current_slot.saturating_add(1), self.epoch_randomness);
        true
    }

    fn randomness_for_slot(&self, slot: Slot) -> H256 {
        if slot > self.current_slot {
            let mut seed = self.epoch_randomness;
            let mut epoch = self.current_epoch;
            let mut updated = self.epoch_randomness_updated;
            while epoch < slot / self.epoch_length {
                let boundary_slot = epoch.saturating_add(1).saturating_mul(self.epoch_length);
                if !updated {
                    let mut hasher = Sha256::new();
                    hasher.update(seed.as_bytes());
                    hasher.update(boundary_slot.to_le_bytes());
                    hasher.update(epoch.to_le_bytes());
                    seed = H256::from(<[u8; 32]>::from(hasher.finalize()));
                }
                epoch = epoch.saturating_add(1);
                updated = false;
            }
            return seed;
        }
        if let Some((_, seed)) = self.randomness_history.range(..=slot).next_back() {
            return *seed;
        }
        self.epoch_randomness
    }

    /// Whether epoch randomness has already been updated from a real VRF output.
    pub fn epoch_randomness_updated(&self) -> bool {
        self.epoch_randomness_updated
    }

    /// Process a vote and check for quorum.
    ///
    /// Safety properties enforced:
    /// 1. Vote deduplication — one vote per validator per (slot, phase, block)
    /// 2. Stake verification — claimed stake must match validator registry
    /// 3. Unknown validator rejection
    pub fn process_vote(&mut self, vote: Vote) -> Result<Option<QuorumCertificate>> {
        // Accept votes for recent slots (and one slot of clock skew ahead).
        if vote.slot > self.current_slot.saturating_add(1)
            || vote.slot.saturating_add(VOTE_WINDOW_SLOTS) < self.current_slot
        {
            bail!(
                "vote for slot {} outside window around current slot {}",
                vote.slot,
                self.current_slot
            );
        }

        let voter_addr = vote.validator.to_address();

        // Verify voter is a known validator in the epoch-frozen set.
        // Using the epoch snapshot ensures mid-epoch slashing doesn't change
        // who can vote or their stake weight within the current epoch.
        let registered = self
            .epoch_validators
            .get(&voter_addr)
            .ok_or_else(|| anyhow::anyhow!("unknown validator: {:?}", voter_addr))?;

        // Verify claimed stake matches registry
        if vote.stake != registered.stake {
            bail!(
                "claimed stake {} != registered stake {} for {:?}",
                vote.stake,
                registered.stake,
                voter_addr
            );
        }

        // Verify BLS signature FIRST (before equivocation check, to prevent
        // an attacker from poisoning the equivocation record with invalid-sig votes).
        // Mandatory: every validator MUST have a registered BLS key, and every vote
        // MUST carry a valid 96-byte BLS signature.
        let bls_pk = self.bls_pubkeys.get(&voter_addr).ok_or_else(|| {
            anyhow::anyhow!(
                "no BLS public key registered for validator {:?}",
                voter_addr
            )
        })?;
        if bls_pk.len() != 48 {
            bail!(
                "registered BLS pubkey has invalid length {} for {:?}",
                bls_pk.len(),
                voter_addr
            );
        }
        let vote_msg = {
            let mut msg = Vec::new();
            msg.extend_from_slice(vote.block_hash.as_bytes());
            msg.extend_from_slice(&vote.slot.to_le_bytes());
            msg
        };
        let sig_bytes = vote.signature.as_bytes();
        if sig_bytes.len() != 96 {
            bail!(
                "vote signature has invalid length {} from {:?}",
                sig_bytes.len(),
                voter_addr
            );
        }
        match aether_crypto_bls::keypair::verify(bls_pk, &vote_msg, sig_bytes) {
            Ok(true) => {} // Valid signature
            Ok(false) => bail!("invalid BLS signature on vote from {:?}", voter_addr),
            Err(e) => bail!("BLS verification error for {:?}: {e}", voter_addr),
        }

        // Equivocation detection: check if this validator already voted for a DIFFERENT block
        // at this slot. Only checked AFTER signature verification so invalid-sig votes
        // can't poison the record.
        // Uses entry() API for atomic check-then-insert (no TOCTOU race).
        let vote_key = (vote.slot, voter_addr);
        match self.vote_record.entry(vote_key) {
            Entry::Occupied(e) => {
                if *e.get() != vote.block_hash {
                    tracing::warn!(
                        validator = ?voter_addr,
                        first_block = ?e.get(),
                        second_block = ?vote.block_hash,
                        slot = vote.slot,
                        "EQUIVOCATION: validator double-voted in same slot"
                    );
                    bail!(
                        "equivocation detected: validator {:?} double-voted at slot {}",
                        voter_addr,
                        vote.slot
                    );
                }
            }
            Entry::Vacant(e) => {
                e.insert(vote.block_hash);
            }
        }

        // Bound vote storage: limit unique block hashes per (slot, phase) to prevent
        // memory exhaustion from adversarial blocks. Validators can propose at most
        // validator_count blocks, so 2x that is generous.
        // Every vote is a single-round vote for one block at one slot, so votes
        // are keyed under the Propose phase regardless of local phase state.
        let max_block_hashes_per_phase = self.validators.len() * 2;
        let existing_hashes: usize = self
            .votes
            .keys()
            .filter(|(s, p, _)| *s == vote.slot && *p == Phase::Propose)
            .count();
        let key = (vote.slot, Phase::Propose, vote.block_hash);
        if !self.votes.contains_key(&key) && existing_hashes >= max_block_hashes_per_phase {
            bail!(
                "too many block candidates for slot {} (limit: {})",
                vote.slot,
                max_block_hashes_per_phase
            );
        }

        // Deduplicate: one vote per validator per (slot, phase, block)
        let votes_map = self.votes.entry(key.clone()).or_default();
        if votes_map.contains_key(&voter_addr) {
            return Ok(None);
        }
        votes_map.insert(voter_addr, vote.clone());

        // Check for quorum (2/3+ stake)
        let voted_stake: u128 = votes_map
            .values()
            .map(|v| v.stake)
            .fold(0u128, u128::saturating_add);
        // Use epoch-frozen total stake for quorum calculation so mid-epoch
        // slashing cannot lower the quorum threshold within an epoch.
        let has_quorum = crate::has_quorum(voted_stake, self.epoch_total_stake);

        if has_quorum {
            // Single-validator fast path
            if self.epoch_validators.len() == 1 {
                let qc = QuorumCertificate {
                    slot: vote.slot,
                    block_hash: vote.block_hash,
                    phase: self.current_phase.clone(),
                    total_stake: vote.stake,
                    signers: vec![voter_addr],
                    aggregated_signature: vote.signature.as_bytes().to_vec(),
                    aggregated_pubkey: vec![],
                };
                self.qcs.insert(key, qc.clone());
                let wire = self.wire_qc(&qc);
                self.record_certified(vote.block_hash, vote.slot, wire);
                // A single validator is its own quorum: certified == final.
                if vote.slot > self.finalized_slot {
                    self.finalized_slot = vote.slot;
                    self.finalized_block = Some(vote.block_hash);
                }
                self.pacemaker.on_commit();
                return Ok(Some(qc));
            }

            // Multi-validator: aggregate votes
            let votes_vec: Vec<Vote> = votes_map.values().cloned().collect();
            let qc = self.aggregate_votes(&votes_vec)?;
            self.qcs.insert(key, qc.clone());

            // Record the certified block. This updates the lock / high QC and
            // applies the 2-chain commit rule (see `try_commit_parent_of`).
            let wire = self.wire_qc(&qc);
            self.record_certified(vote.block_hash, vote.slot, wire);
            self.pacemaker.on_commit();
            return Ok(Some(qc));
        }

        Ok(None)
    }

    /// Process a batch of votes with a single multi-pairing BLS verification.
    ///
    /// Batch verification via `verify_batch` collapses N pairing checks into
    /// one final exponentiation using random linear combination (Bellare et al.
    /// 2007).  This is ~N× faster than N individual `process_vote` calls when
    /// the bottleneck is BLS pairing arithmetic.
    ///
    /// Security invariant: all BLS signatures are verified before any
    /// equivocation check, maintaining the same ordering as `process_vote`.
    /// If the batch check fails, we fall back to individual verification to
    /// identify and reject the bad vote(s).
    pub fn batch_process_votes(
        &mut self,
        votes: Vec<Vote>,
    ) -> Result<Vec<Option<QuorumCertificate>>> {
        if votes.is_empty() {
            return Ok(vec![]);
        }

        // Pre-validate and prepare BLS verification tuples
        struct Prepared {
            vote: Vote,
            pk: Vec<u8>,
            msg: Vec<u8>,
        }
        let mut prepared = Vec::with_capacity(votes.len());
        let mut rejected: Vec<(usize, String)> = Vec::new();

        for (i, vote) in votes.iter().enumerate() {
            if vote.slot > self.current_slot.saturating_add(1)
                || vote.slot.saturating_add(VOTE_WINDOW_SLOTS) < self.current_slot
            {
                rejected.push((i, format!("wrong slot: {}", vote.slot)));
                continue;
            }
            let addr = vote.validator.to_address();
            let registered = match self.epoch_validators.get(&addr) {
                Some(v) => v,
                None => {
                    rejected.push((i, format!("unknown validator: {:?}", addr)));
                    continue;
                }
            };
            if vote.stake != registered.stake {
                rejected.push((
                    i,
                    format!("stake mismatch: {} vs {}", vote.stake, registered.stake),
                ));
                continue;
            }
            let bls_pk = match self.bls_pubkeys.get(&addr) {
                Some(pk) if pk.len() == 48 => pk.clone(),
                _ => {
                    rejected.push((i, format!("no valid BLS pubkey for {:?}", addr)));
                    continue;
                }
            };
            let sig_bytes = vote.signature.as_bytes();
            if sig_bytes.len() != 96 {
                rejected.push((i, format!("bad signature length: {}", sig_bytes.len())));
                continue;
            }

            let mut msg = Vec::with_capacity(40);
            msg.extend_from_slice(vote.block_hash.as_bytes());
            msg.extend_from_slice(&vote.slot.to_le_bytes());

            prepared.push(Prepared {
                vote: vote.clone(),
                pk: bls_pk,
                msg,
            });
        }

        // Batch-verify all BLS signatures in a single multi-pairing check
        let tuples: Vec<(&[u8], &[u8], &[u8])> = prepared
            .iter()
            .map(|p| {
                (
                    p.pk.as_slice(),
                    p.msg.as_slice(),
                    p.vote.signature.as_bytes(),
                )
            })
            .collect();

        let batch_ok = if tuples.is_empty() {
            true
        } else {
            aether_crypto_bls::verify_batch(&tuples)?
        };

        // If batch fails, fall back to individual verification to identify bad votes
        let sig_valid: Vec<bool> = if batch_ok {
            vec![true; prepared.len()]
        } else {
            prepared
                .iter()
                .map(|p| {
                    aether_crypto_bls::keypair::verify(&p.pk, &p.msg, p.vote.signature.as_bytes())
                        .unwrap_or(false)
                })
                .collect()
        };

        // Process each valid vote through the remaining logic (equivocation, dedup, quorum)
        let mut results = vec![None; votes.len()];
        let mut prepared_idx = 0;
        for (i, _vote) in votes.iter().enumerate() {
            if rejected.iter().any(|(ri, _)| *ri == i) {
                continue;
            }
            if !sig_valid[prepared_idx] {
                prepared_idx += 1;
                continue;
            }
            // Signature valid — feed through the remaining process_vote logic
            // (equivocation detection, dedup, quorum).
            // Since the BLS check is already done, we call process_vote which
            // will re-verify (harmless; the hot path is the batch check above).
            // In a future optimization we could split process_vote into
            // verify + apply, but correctness > micro-optimization.
            if let Ok(qc) = self.process_vote(prepared[prepared_idx].vote.clone()) {
                results[i] = qc;
            }
            prepared_idx += 1;
        }

        Ok(results)
    }

    /// Aggregate BLS signatures from votes
    fn aggregate_votes(&self, votes: &[Vote]) -> Result<QuorumCertificate> {
        let signatures: Vec<Vec<u8>> = votes
            .iter()
            .map(|v| v.signature.as_bytes().to_vec())
            .collect();

        // Use registered BLS public keys (48 bytes) — NOT Ed25519 keys (32 bytes)
        let pubkeys: Vec<Vec<u8>> = votes
            .iter()
            .map(|v| {
                let addr = v.validator.to_address();
                // First try BLS pubkey registry (correct 48-byte keys)
                if let Some(bls_pk) = self.bls_pubkeys.get(&addr) {
                    bls_pk.clone()
                } else {
                    // Fallback: pad Ed25519 key (will produce invalid BLS verification
                    // but prevents panic — votes from unregistered validators were
                    // already rejected by process_vote)
                    vec![0u8; 48]
                }
            })
            .collect();

        let agg_sig = aggregate_signatures(&signatures)?;
        let agg_pk = aggregate_public_keys(&pubkeys)?;

        let total_stake = votes
            .iter()
            .map(|v| v.stake)
            .fold(0u128, u128::saturating_add);
        let signers: Vec<Address> = votes.iter().map(|v| v.validator.to_address()).collect();

        Ok(QuorumCertificate {
            slot: votes[0].slot,
            block_hash: votes[0].block_hash,
            phase: self.current_phase.clone(),
            total_stake,
            signers,
            aggregated_signature: agg_sig,
            aggregated_pubkey: agg_pk,
        })
    }

    /// Advance to next HotStuff phase
    pub fn advance_phase(&mut self) {
        self.current_phase = match self.current_phase {
            Phase::Propose => Phase::Prevote,
            Phase::Prevote => Phase::Precommit,
            Phase::Precommit => Phase::Commit,
            Phase::Commit => Phase::Propose,
        };
    }

    pub fn current_phase(&self) -> &Phase {
        &self.current_phase
    }
}

impl crate::Finality for HybridConsensus {
    fn check_finality(&mut self, slot: Slot) -> bool {
        if slot <= self.finalized_slot && slot > self.last_reported_finalized {
            self.last_reported_finalized = slot;
            true
        } else {
            false
        }
    }

    fn finalized_slot(&self) -> Slot {
        self.finalized_slot
    }

    fn record_block(&mut self, block_hash: H256, parent_hash: H256, slot: Slot) {
        self.block_parents.insert(block_hash, parent_hash);
        self.block_slots.insert(block_hash, slot);
        // Votes (and so the QC) for a block can arrive before the block itself;
        // re-check the commit rule now that its parent is known.
        self.try_commit_parent_of(block_hash);
    }
}

impl ConsensusEngine for HybridConsensus {
    fn current_slot(&self) -> Slot {
        self.current_slot
    }

    fn advance_slot(&mut self) {
        self.current_slot = self.current_slot.saturating_add(1);
        self.current_phase = Phase::Propose;
        // Keep votes for recent slots: they may still complete a quorum.
        let vote_floor = self.current_slot.saturating_sub(VOTE_WINDOW_SLOTS);
        self.votes.retain(|(slot, _, _), _| *slot >= vote_floor);

        // Prune old consensus state to bound memory growth.
        // Keep only data from the last 200 slots.
        let prune_before = self.current_slot.saturating_sub(200);
        let finalized_block = self.finalized_block;
        let high = self.high_qc.map(|(h, _)| h);
        self.certified.retain(|hash, c| {
            c.slot >= prune_before || Some(*hash) == finalized_block || Some(*hash) == high
        });
        self.vote_record
            .retain(|(slot, _), _| *slot >= prune_before);
        self.qcs.retain(|(slot, _, _), _| *slot >= prune_before);
        // Prune block parent tracking for very old blocks
        self.block_slots.retain(|_, slot| *slot >= prune_before);
        let slots_to_keep: std::collections::HashSet<&H256> = self.block_slots.keys().collect();
        self.block_parents.retain(|k, _| slots_to_keep.contains(k));

        // Check for epoch transition
        if self.epoch_length > 0 && self.current_slot % self.epoch_length == 0 {
            // If no real VRF output arrived this epoch, apply deterministic fallback.
            if !self.epoch_randomness_updated {
                let mut hasher = Sha256::new();
                hasher.update(self.epoch_randomness.as_bytes());
                hasher.update(self.current_slot.to_le_bytes());
                hasher.update(self.current_epoch.to_le_bytes());
                let new_randomness = hasher.finalize();
                self.epoch_randomness = H256::from(<[u8; 32]>::from(new_randomness));
            }
            self.epoch_randomness_updated = false;
            self.current_epoch = self.current_epoch.saturating_add(1);
            self.randomness_history
                .insert(self.current_slot, self.epoch_randomness);
            let oldest_slot = self.current_slot.saturating_sub(10_000 * self.epoch_length);
            self.randomness_history
                .retain(|slot, _| *slot >= oldest_slot);

            // Protocol v2 keeps the genesis committee fixed across epochs.
            // Slashing changes economic stake immediately, but committee
            // reconfiguration requires finalized on-chain state and is not yet
            // implemented; local slash arrival order must not change consensus.
        }
    }

    fn is_leader(&self, slot: Slot, validator_pubkey: &PublicKey) -> bool {
        // For now, just check if the validator address matches
        // In full implementation, we'd verify the VRF proof
        if let Some(my_addr) = &self.my_address {
            let validator_addr = validator_pubkey.to_address();
            return validator_addr == *my_addr && self.check_my_eligibility(slot).is_some();
        }
        false
    }

    fn validate_block(&self, block: &Block) -> Result<()> {
        self.verify_proposal(block)?;
        if !self.safe_to_vote(block) {
            let parent_slot = self
                .block_slots
                .get(&block.header.parent_hash)
                .copied()
                .unwrap_or(0);
            bail!(
                "block does not extend locked block and parent slot {} < locked slot {} \
                 (safe node predicate failed)",
                parent_slot,
                self.locked_slot
            );
        }
        Ok(())
    }

    fn verify_proposal(&self, block: &Block) -> Result<()> {
        // Check slot is valid (one slot of tolerance for clock skew between
        // validator processes that tick on their own wall-clock timers).
        if block.header.slot > self.current_slot.saturating_add(1) {
            bail!("block from future slot");
        }

        // Validate timestamp: must be reasonable (within 1 hour of expected)
        let expected_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let max_drift: u64 = 3600; // 1 hour tolerance
        if block.header.timestamp > expected_timestamp + max_drift {
            bail!(
                "block timestamp {} is too far in the future (now={})",
                block.header.timestamp,
                expected_timestamp
            );
        }

        // Verify VRF proof and leader eligibility
        if !self.verify_leader_eligibility(block)? {
            bail!("invalid leader proof");
        }
        Ok(())
    }

    /// HotStuff safe node predicate: vote for a block only if it extends the
    /// locked block, or if its parent is at a slot >= our locked slot (a
    /// quorum has moved past our lock).
    fn safe_to_vote(&self, block: &Block) -> bool {
        match &self.locked_block {
            None => true,
            Some(locked) if block.header.parent_hash == *locked => true,
            Some(_) => {
                let parent_slot = self
                    .block_slots
                    .get(&block.header.parent_hash)
                    .copied()
                    .unwrap_or(0);
                parent_slot >= self.locked_slot
            }
        }
    }

    fn add_vote(&mut self, vote: Vote) -> Result<()> {
        self.process_vote(vote)?;
        Ok(())
    }

    fn total_stake(&self) -> u128 {
        self.total_stake
    }

    fn get_leader_proof(&self, slot: Slot) -> Option<VrfProof> {
        self.check_my_eligibility(slot)
    }

    fn update_epoch_randomness(&mut self, vrf_output: &[u8; 32]) -> bool {
        HybridConsensus::update_epoch_randomness(self, vrf_output)
    }

    fn validator_stake(&self, address: &Address) -> u128 {
        self.validators.get(address).map_or(0, |v| v.stake)
    }

    fn validator_stake_at_slot(&self, address: &Address, slot: Slot) -> u128 {
        let _ = slot;
        self.epoch_validators.get(address).map_or(0, |v| v.stake)
    }

    fn total_stake_at_slot(&self, slot: Slot) -> u128 {
        let _ = slot;
        self.epoch_total_stake
    }

    fn is_timed_out(&self) -> bool {
        self.pacemaker.is_timed_out()
    }

    fn on_timeout(&mut self) {
        self.pacemaker.on_timeout();
        // On timeout, reset to Propose phase for the next slot. Collected votes
        // are kept: a late quorum for a recent block is still valid.
        self.current_phase = Phase::Propose;
    }

    fn advance_pacemaker_to_round(&mut self, round: u64) {
        self.pacemaker.advance_to_round(round);
    }

    fn get_bls_pubkey(&self, address: &aether_types::Address) -> Option<Vec<u8>> {
        self.bls_pubkeys.get(address).cloned()
    }

    fn register_bls_pubkey(
        &mut self,
        address: aether_types::Address,
        bls_pubkey: Vec<u8>,
        pop_signature: &[u8],
    ) -> Result<()> {
        self.register_bls_pubkey(address, bls_pubkey, pop_signature)
    }

    fn slash_validator(&mut self, address: &Address, slash_bps: u128) -> u128 {
        if let Some(validator) = self.validators.get_mut(address) {
            let slash_amount = mul_div(validator.stake, slash_bps, 10000);
            validator.stake = validator.stake.saturating_sub(slash_amount);
            self.total_stake = self.total_stake.saturating_sub(slash_amount);
            slash_amount
        } else {
            0
        }
    }

    fn validator_addresses_and_stakes(&self) -> Vec<(Address, u128)> {
        self.validators
            .iter()
            .filter(|(_, v)| v.stake > 0)
            .map(|(addr, v)| (*addr, v.stake))
            .collect()
    }

    fn validator_addresses_and_stakes_at_epoch(&self, epoch: u64) -> Vec<(Address, u128)> {
        let _ = epoch;
        self.epoch_validators
            .iter()
            .filter(|(_, validator)| validator.stake > 0)
            .map(|(address, validator)| (*address, validator.stake))
            .collect()
    }

    fn total_stake_at_epoch(&self, epoch: u64) -> u128 {
        let _ = epoch;
        self.epoch_total_stake
    }

    fn on_qc(&mut self, qc: &AggregatedVote) {
        self.record_certified(qc.block_hash, qc.slot, qc.clone());
    }

    fn qc_for(&self, block_hash: &H256) -> Option<AggregatedVote> {
        self.certified.get(block_hash).map(|c| c.qc.clone())
    }

    fn high_qc(&self) -> Option<(H256, Slot)> {
        self.high_qc
    }

    fn finalized_block(&self) -> Option<H256> {
        self.finalized_block
    }
}
