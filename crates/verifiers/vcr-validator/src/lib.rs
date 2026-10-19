// ============================================================================
// AETHER VCR VALIDATOR - Verifiable Compute Receipt
// ============================================================================
// PURPOSE: Verify AI inference computations are deterministic and correct
//
// VCR COMPONENTS:
// 1. Trace commitment: KZG commitment to execution trace
// 2. TEE attestation: Proof of secure execution
// 3. Input/output hashes: Deterministic I/O
// 4. Metadata: Model hash, timestamp, worker ID
//
// VERIFICATION PROCESS:
// 1. Check TEE attestation (worker ran in TEE)
// 2. Verify KZG commitment (trace matches claimed output)
// 3. Challenge mechanism (spot-check trace validity)
// 4. Worker signature verification
// ============================================================================

use aether_crypto_kzg::{KzgCommitment, KzgProof, KzgVerifier};
use aether_crypto_primitives::ed25519;
use aether_types::H256;
use aether_verifiers_tee::{AttestationReport, TeeVerifier};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiableComputeReceipt {
    pub job_id: H256,
    pub worker_id: Vec<u8>,
    pub model_hash: H256,
    pub input_hash: H256,
    pub output_hash: H256,
    pub trace_commitment: Vec<u8>, // KZG commitment (48 bytes)
    #[serde(default)]
    pub trace_proof: Vec<u8>, // KZG opening proof (48 bytes)
    #[serde(default)]
    pub trace_evaluation: Vec<u8>, // Claimed trace evaluation (32 bytes)
    #[serde(default)]
    pub trace_point: Vec<u8>, // Challenge point (32 bytes)
    pub tee_attestation: Vec<u8>,  // JSON-encoded AttestationReport
    pub timestamp: u64,
    pub signature: Vec<u8>, // Ed25519 signature from worker public key
}

pub struct VcrValidator {
    /// Minimum quorum size for consensus
    quorum_size: usize,

    /// Challenge window (slots)
    challenge_window: u64,

    /// TEE attestation verifier
    tee_verifier: TeeVerifier,

    /// KZG verifier for trace checks
    kzg_verifier: KzgVerifier,
}

impl VcrValidator {
    /// Create a VCR validator with explicit configuration.
    /// Use `new_for_test()` for development/testing only.
    pub fn new(
        kzg_verifier: KzgVerifier,
        tee_verifier: TeeVerifier,
        quorum_size: usize,
        challenge_window: u64,
    ) -> Self {
        VcrValidator {
            quorum_size,
            challenge_window,
            tee_verifier,
            kzg_verifier,
        }
    }

    /// Create a VCR validator for development/testing with insecure defaults.
    /// WARNING: Do NOT use in production — uses test KZG parameters and
    /// accepts the default simulation TEE measurement.
    pub fn new_for_test() -> Self {
        let mut tee_verifier = TeeVerifier::new();
        tee_verifier.add_approved_measurement(vec![1u8; 48]);

        VcrValidator {
            quorum_size: 3,
            challenge_window: 10,
            tee_verifier,
            kzg_verifier: KzgVerifier::new_insecure_test(1024),
        }
    }

    pub fn approve_measurement(&mut self, measurement: Vec<u8>) {
        self.tee_verifier.add_approved_measurement(measurement);
    }

    /// Verify a single VCR
    pub fn verify(&self, vcr: &VerifiableComputeReceipt) -> Result<()> {
        // 1. Verify basic fields
        if vcr.worker_id.len() != 32 {
            bail!("worker ID must be a 32-byte ed25519 public key");
        }

        // 2. Verify TEE attestation
        self.verify_attestation(vcr)?;

        // 3. Verify KZG commitment opening
        self.verify_trace_opening(vcr)?;

        // 4. Verify worker signature
        self.verify_signature(vcr)?;

        Ok(())
    }

    /// Verify VCRs from multiple workers (quorum consensus).
    ///
    /// Only VCRs that agree on the majority output are verified and counted
    /// toward quorum. Dissenting VCRs are ignored — a single invalid dissenter
    /// cannot poison a valid quorum. Workers must be unique (Sybil protection).
    pub fn verify_quorum(&self, vcrs: &[VerifiableComputeReceipt]) -> Result<()> {
        if vcrs.len() < self.quorum_size {
            bail!("insufficient quorum: {} < {}", vcrs.len(), self.quorum_size);
        }

        // All VCRs should have same job_id
        let job_id = vcrs[0].job_id;
        for vcr in vcrs {
            if vcr.job_id != job_id {
                bail!("mismatched job IDs in quorum");
            }
        }

        // Sybil protection: reject duplicate worker IDs
        let mut seen_workers = std::collections::HashSet::new();
        for vcr in vcrs {
            if !seen_workers.insert(&vcr.worker_id) {
                bail!("duplicate worker ID in quorum — possible Sybil attack");
            }
        }

        // Find the majority output hash by counting occurrences
        let mut counts: std::collections::HashMap<H256, usize> = std::collections::HashMap::new();
        for vcr in vcrs {
            *counts.entry(vcr.output_hash).or_insert(0) += 1;
        }
        let (&majority_output, &majority_count) = counts
            .iter()
            .max_by_key(|(_, count)| *count)
            .ok_or_else(|| anyhow::anyhow!("empty output set in quorum verification"))?;

        // Check 2/3 consensus on the majority output
        if majority_count * 3 < vcrs.len() * 2 {
            bail!(
                "no consensus: {} / {} agree on majority output",
                majority_count,
                vcrs.len()
            );
        }

        // Only verify VCRs that agree with the majority — dissenters are
        // ignored so one invalid VCR cannot poison the entire quorum.
        let mut verified_count = 0;
        for vcr in vcrs {
            if vcr.output_hash != majority_output {
                continue;
            }
            self.verify(vcr)?;
            verified_count += 1;
        }

        // Ensure enough verified VCRs meet the quorum threshold
        if verified_count < self.quorum_size {
            bail!(
                "insufficient verified quorum: {} < {}",
                verified_count,
                self.quorum_size
            );
        }

        Ok(())
    }

    fn verify_attestation(&self, vcr: &VerifiableComputeReceipt) -> Result<()> {
        let report: AttestationReport = serde_json::from_slice(&vcr.tee_attestation)
            .context("invalid tee_attestation payload (expected JSON AttestationReport)")?;
        let now = current_timestamp();
        self.tee_verifier
            .verify(&report, now)
            .context("TEE attestation verification failed")
    }

    fn verify_trace_opening(&self, vcr: &VerifiableComputeReceipt) -> Result<()> {
        let commitment = KzgCommitment {
            commitment: vcr.trace_commitment.clone(),
        };
        let proof = KzgProof {
            proof: vcr.trace_proof.clone(),
            evaluation: vcr.trace_evaluation.clone(),
        };

        let point: [u8; 32] = vcr
            .trace_point
            .as_slice()
            .try_into()
            .context("trace_point must be 32 bytes")?;
        let valid = self
            .kzg_verifier
            .verify(&commitment, &proof, &point)
            .context("KZG trace proof verification failed")?;
        anyhow::ensure!(valid, "KZG trace proof verification returned false");
        Ok(())
    }

    fn verify_signature(&self, vcr: &VerifiableComputeReceipt) -> Result<()> {
        if vcr.signature.is_empty() {
            bail!("empty signature");
        }

        let message = vcr.signing_message()?;
        ed25519::verify(&vcr.worker_id, &message, &vcr.signature)
            .map_err(|e| anyhow::anyhow!("signature verification failed: {e}"))
    }

    pub fn set_quorum_size(&mut self, size: usize) {
        self.quorum_size = size;
    }

    pub fn challenge_window(&self) -> u64 {
        self.challenge_window
    }
}

impl VerifiableComputeReceipt {
    /// Compute the deterministic signing message using direct hash construction.
    /// This avoids bincode's non-canonical serialization which could differ across versions.
    fn signing_message(&self) -> Result<Vec<u8>> {
        let mut hasher = Sha256::new();
        hasher.update(b"VCR-v1"); // Version domain separator
        hasher.update(self.job_id.as_bytes());
        hasher.update(&self.worker_id);
        hasher.update(self.model_hash.as_bytes());
        hasher.update(self.input_hash.as_bytes());
        hasher.update(self.output_hash.as_bytes());
        hasher.update(&self.trace_commitment);
        hasher.update(&self.trace_proof);
        hasher.update(&self.trace_evaluation);
        hasher.update(&self.trace_point);
        hasher.update(&self.tee_attestation);
        hasher.update(self.timestamp.to_le_bytes());
        Ok(hasher.finalize().to_vec())
    }
}

impl Default for VcrValidator {
    fn default() -> Self {
        Self::new_for_test()
    }
}

fn current_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
