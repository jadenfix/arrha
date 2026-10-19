// ============================================================================
// AETHER JOB ESCROW - AI Inference Job Management
// ============================================================================
// PURPOSE: Escrow AIC tokens for AI inference requests
//
// FLOW:
// 1. User posts job with AIC deposit
// 2. Provider accepts job
// 3. Provider submits result + VCR
// 4. Validators verify VCR
// 5. Escrow releases payment (burns AIC)
// 6. User receives result
//
// JOB STATES:
// - Posted: Awaiting provider
// - Accepted: Provider working
// - Submitted: Result pending verification
// - Verified: VCR confirmed, payment released
// - Disputed: Challenge active
// - Completed: Final state
// - Cancelled: Refunded
//
// SECURITY:
// - VCR verification required
// - Challenge period (10 slots)
// - Reputation scoring
// - Slashing for invalid results
// ============================================================================

use aether_types::{Address, H256};
use aether_verifiers_vcr::{VcrValidator, VerifiableComputeReceipt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum JobStatus {
    Posted,
    Accepted,
    Submitted,
    Verified,
    Disputed,
    Completed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub job_id: H256,
    pub requester: Address,
    pub provider: Option<Address>,
    pub model_hash: H256,
    pub input_hash: H256,
    pub output_hash: Option<H256>,
    pub vcr_proof: Option<Vec<u8>>,
    pub payment: u128,
    pub status: JobStatus,
    pub posted_slot: u64,
    pub deadline_slot: u64,
    pub challenge_end_slot: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobEscrowState {
    pub jobs: HashMap<H256, Job>,
    pub provider_reputation: HashMap<Address, i32>,
    pub requester_escrow: HashMap<Address, u128>,
    pub provider_claimable: HashMap<Address, u128>,
    pub total_jobs: u64,
    pub completed_jobs: u64,
}

impl JobEscrowState {
    pub fn new() -> Self {
        JobEscrowState {
            jobs: HashMap::new(),
            provider_reputation: HashMap::new(),
            requester_escrow: HashMap::new(),
            provider_claimable: HashMap::new(),
            total_jobs: 0,
            completed_jobs: 0,
        }
    }

    /// Post a new job
    #[allow(clippy::too_many_arguments)]
    pub fn post_job(
        &mut self,
        job_id: H256,
        requester: Address,
        model_hash: H256,
        input_hash: H256,
        payment: u128,
        current_slot: u64,
        deadline_slots: u64,
    ) -> Result<(), String> {
        if self.jobs.contains_key(&job_id) {
            return Err("job already exists".to_string());
        }

        if payment == 0 {
            return Err("payment must be non-zero".to_string());
        }

        let job = Job {
            job_id,
            requester,
            provider: None,
            model_hash,
            input_hash,
            output_hash: None,
            vcr_proof: None,
            payment,
            status: JobStatus::Posted,
            posted_slot: current_slot,
            deadline_slot: current_slot
                .checked_add(deadline_slots)
                .ok_or_else(|| "slot overflow in deadline calculation".to_string())?,
            challenge_end_slot: None,
        };

        self.jobs.insert(job_id, job);
        let escrowed = self.requester_escrow.entry(requester).or_insert(0);
        *escrowed = escrowed
            .checked_add(payment)
            .ok_or("requester escrow overflow")?;
        self.total_jobs = self
            .total_jobs
            .checked_add(1)
            .ok_or("total_jobs overflow")?;

        Ok(())
    }

    /// Minimum provider reputation required to accept a job.
    ///
    /// Providers whose reputation score is at or below this threshold have been
    /// penalised sufficiently that they are barred from taking new work.  The
    /// coordinator independently bans providers at -100, but the on-chain
    /// escrow enforces the same floor so a compromised off-chain coordinator
    /// cannot bypass it.
    pub const MIN_PROVIDER_REPUTATION: i32 = -50;

    /// Provider accepts job
    pub fn accept_job(&mut self, job_id: H256, provider: Address) -> Result<(), String> {
        // Reject providers whose reputation is too low.
        let reputation = self.get_provider_reputation(&provider);
        if reputation <= Self::MIN_PROVIDER_REPUTATION {
            return Err(format!(
                "provider reputation {} is too low to accept jobs (minimum {})",
                reputation,
                Self::MIN_PROVIDER_REPUTATION
            ));
        }

        let job = self.jobs.get_mut(&job_id).ok_or("job not found")?;

        if job.status != JobStatus::Posted {
            return Err("job not available".to_string());
        }

        // A requester must not be able to act as provider for their own job —
        // doing so would let them steal the escrowed payment.
        if provider == job.requester {
            return Err("provider cannot be the same address as the job requester".to_string());
        }

        job.provider = Some(provider);
        job.status = JobStatus::Accepted;

        Ok(())
    }

    /// Provider submits result
    pub fn submit_result(
        &mut self,
        job_id: H256,
        provider: Address,
        output_hash: H256,
        vcr_proof: Vec<u8>,
        current_slot: u64,
    ) -> Result<(), String> {
        let job = self.jobs.get_mut(&job_id).ok_or("job not found")?;

        if job.provider != Some(provider) {
            return Err("not job provider".to_string());
        }

        if job.status != JobStatus::Accepted {
            return Err("invalid job status".to_string());
        }

        if current_slot > job.deadline_slot {
            return Err("deadline passed".to_string());
        }

        job.output_hash = Some(output_hash);
        job.vcr_proof = Some(vcr_proof);
        job.status = JobStatus::Submitted;
        job.challenge_end_slot = Some(
            current_slot
                .checked_add(10)
                .ok_or_else(|| "slot overflow in challenge period calculation".to_string())?,
        ); // 10 slot challenge period

        Ok(())
    }

    /// Verify and complete job.
    ///
    /// `vcr_validator` is used to cryptographically verify the stored VCR proof
    /// (TEE attestation + KZG trace commitment + worker signature).  The job
    /// transitions to `Completed` only when verification passes.
    pub fn verify_job(
        &mut self,
        job_id: H256,
        current_slot: u64,
        vcr_validator: &VcrValidator,
    ) -> Result<Option<(Address, u128)>, String> {
        let (requester, provider, payment) = {
            let job = self.jobs.get_mut(&job_id).ok_or("job not found")?;

            if job.status != JobStatus::Submitted {
                return Err("job not submitted".to_string());
            }

            // Check challenge period ended
            if let Some(challenge_end) = job.challenge_end_slot {
                if current_slot <= challenge_end {
                    return Err("challenge period not ended".to_string());
                }
            }

            // Cryptographically verify the VCR proof (TEE attestation, KZG trace
            // commitment, and worker signature) before releasing payment.
            let proof_bytes = job.vcr_proof.as_deref().ok_or("missing VCR proof")?;
            let receipt: VerifiableComputeReceipt = serde_json::from_slice(proof_bytes)
                .map_err(|e| format!("invalid VCR proof encoding: {e}"))?;
            vcr_validator
                .verify(&receipt)
                .map_err(|e| format!("VCR proof verification failed: {e}"))?;

            let provider = job.provider.ok_or("job has no provider")?;
            let requester = job.requester;
            let payment = job.payment;
            (requester, provider, payment)
        };

        let escrowed = self
            .requester_escrow
            .get_mut(&requester)
            .ok_or("missing requester escrow balance")?;
        if *escrowed < payment {
            return Err("insufficient requester escrow balance".to_string());
        }
        *escrowed = escrowed.checked_sub(payment).ok_or("escrow underflow")?;
        let remove_requester_escrow = *escrowed == 0;
        if remove_requester_escrow {
            self.requester_escrow.remove(&requester);
        }
        let claimable = self.provider_claimable.entry(provider).or_insert(0);
        *claimable = claimable
            .checked_add(payment)
            .ok_or("provider claimable overflow")?;
        let job = self.jobs.get_mut(&job_id).ok_or("job not found")?;
        job.status = JobStatus::Completed;
        let rep = self.provider_reputation.entry(provider).or_insert(0);
        *rep = rep.checked_add(1).ok_or("reputation overflow")?;
        self.completed_jobs = self
            .completed_jobs
            .checked_add(1)
            .ok_or("completed_jobs overflow")?;

        Ok(Some((provider, payment)))
    }

    /// Challenge a result.
    ///
    /// Only the job requester can challenge a submitted result.
    /// This puts the job into Disputed status, preventing automatic verification.
    pub fn challenge_job(&mut self, job_id: H256, challenger: Address) -> Result<(), String> {
        let job = self.jobs.get_mut(&job_id).ok_or("job not found")?;

        if job.status != JobStatus::Submitted {
            return Err("cannot challenge job".to_string());
        }

        // Only the job requester can challenge
        if challenger != job.requester {
            return Err("only job requester can challenge".to_string());
        }

        job.status = JobStatus::Disputed;

        Ok(())
    }

    /// Cancel job (refund requester)
    pub fn cancel_job(&mut self, job_id: H256, caller: Address) -> Result<(), String> {
        let (requester, payment) = {
            let job = self.jobs.get_mut(&job_id).ok_or("job not found")?;

            if caller != job.requester {
                return Err("not job requester".to_string());
            }

            if job.status != JobStatus::Posted {
                return Err("cannot cancel job".to_string());
            }

            let requester = job.requester;
            let payment = job.payment;
            (requester, payment)
        };

        let escrowed = self
            .requester_escrow
            .get_mut(&requester)
            .ok_or("missing requester escrow balance")?;
        if *escrowed < payment {
            return Err("insufficient requester escrow balance".to_string());
        }
        *escrowed = escrowed.checked_sub(payment).ok_or("escrow underflow")?;
        let remove_requester_escrow = *escrowed == 0;
        if remove_requester_escrow {
            self.requester_escrow.remove(&requester);
        }
        let job = self.jobs.get_mut(&job_id).ok_or("job not found")?;
        job.status = JobStatus::Cancelled;

        Ok(())
    }

    pub fn get_job(&self, job_id: &H256) -> Option<&Job> {
        self.jobs.get(job_id)
    }

    pub fn get_provider_reputation(&self, provider: &Address) -> i32 {
        self.provider_reputation.get(provider).copied().unwrap_or(0)
    }

    pub fn escrowed_balance_of(&self, requester: &Address) -> u128 {
        self.requester_escrow.get(requester).copied().unwrap_or(0)
    }

    pub fn claimable_balance_of(&self, provider: &Address) -> u128 {
        self.provider_claimable.get(provider).copied().unwrap_or(0)
    }
}

impl Default for JobEscrowState {
    fn default() -> Self {
        Self::new()
    }
}
