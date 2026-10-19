use aether_types::{Address, H256};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A fraud proof challenging an invalid L2 state transition.
///
/// The challenger re-executes the disputed transaction(s) and shows
/// that the claimed post-state root is incorrect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FraudProof {
    /// The batch being challenged.
    pub batch_id: u64,
    /// Index of the first invalid transaction in the batch.
    pub tx_index: u32,
    /// Pre-state root (should match batch's pre_state_root).
    pub pre_state_root: H256,
    /// Correct post-state root (computed by challenger).
    pub correct_post_state_root: H256,
    /// The batch's claimed (incorrect) post-state root.
    pub claimed_post_state_root: H256,
    /// Challenger's address.
    pub challenger: Address,
    /// Merkle proof of the pre-state at the disputed transaction.
    pub state_proof: Vec<u8>,
    /// The disputed transaction data (for re-execution).
    pub tx_data: Vec<u8>,
}

impl FraudProof {
    pub fn hash(&self) -> H256 {
        // bincode::serialize on a valid struct cannot fail;
        // SHA256 always produces 32 bytes matching H256.
        let bytes = bincode::serialize(self).expect("FraudProof serialization infallible");
        H256::from(<[u8; 32]>::from(Sha256::digest(&bytes)))
    }
}

/// Trait for re-executing L2 transactions to verify fraud proofs.
///
/// Implementations bridge to the actual WASM VM or state transition function.
/// This indirection is required so the rollup crate does not take a hard
/// dependency on `aether-runtime` while still enforcing re-execution.
pub trait ReExecutor {
    /// Execute `tx_data` against the state identified by `pre_state_root`
    /// and return the resulting post-state root.
    ///
    /// Returns `Err` if execution fails (e.g. invalid tx encoding, VM fault).
    fn re_execute(&self, pre_state_root: &H256, tx_data: &[u8]) -> Result<H256, String>;
}

/// Verifies fraud proofs against state commitments.
pub struct FraudProofVerifier {
    /// Bond required to submit a fraud proof (prevents spam).
    pub required_bond: u128,
    /// Reward for successful challenge as a percentage of sequencer's bond (0-100).
    pub challenger_reward_pct: u8,
}

/// Result of fraud proof verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FraudProofResult {
    /// Fraud proof is valid — state transition was incorrect.
    Valid {
        slashed_sequencer: Address,
        challenger_reward: u128,
    },
    /// Fraud proof is invalid — state transition was correct.
    Invalid { reason: String },
}

impl FraudProofVerifier {
    /// Create a new verifier.
    ///
    /// Returns an error if `challenger_reward_pct > 100` to prevent rewards
    /// that exceed the sequencer's bond.
    pub fn new(required_bond: u128, challenger_reward_pct: u8) -> Result<Self, String> {
        if challenger_reward_pct > 100 {
            return Err(format!(
                "challenger_reward_pct {} exceeds 100 — would pay out more than bond",
                challenger_reward_pct
            ));
        }
        Ok(FraudProofVerifier {
            required_bond,
            challenger_reward_pct,
        })
    }

    /// Verify a fraud proof by re-executing the disputed transaction.
    ///
    /// # Arguments
    /// - `proof` — the fraud proof submitted by the challenger.
    /// - `batch_pre_state_root` — the batch's recorded pre-state root (from chain).
    /// - `batch_post_state_root` — the batch's recorded post-state root (from chain).
    /// - `sequencer` — the sequencer who published the batch.
    /// - `sequencer_bond` — the sequencer's current bond balance.
    /// - `challenger_bond` — the bond posted by the challenger (must ≥ `required_bond`).
    /// - `executor` — re-execution engine used to compute the true post-state root.
    #[allow(clippy::too_many_arguments)]
    pub fn verify(
        &self,
        proof: &FraudProof,
        batch_pre_state_root: &H256,
        batch_post_state_root: &H256,
        sequencer: &Address,
        sequencer_bond: u128,
        challenger_bond: u128,
        executor: &dyn ReExecutor,
    ) -> FraudProofResult {
        // Challenger must have posted sufficient bond to prevent spam.
        if challenger_bond < self.required_bond {
            return FraudProofResult::Invalid {
                reason: format!(
                    "challenger bond {} below required {}",
                    challenger_bond, self.required_bond
                ),
            };
        }

        // Pre-state root must match the batch
        if proof.pre_state_root != *batch_pre_state_root {
            return FraudProofResult::Invalid {
                reason: "pre-state root mismatch".into(),
            };
        }

        // Claimed post-state must match the batch's post-state
        if proof.claimed_post_state_root != *batch_post_state_root {
            return FraudProofResult::Invalid {
                reason: "claimed post-state doesn't match batch".into(),
            };
        }

        // The correct state root must differ from the claimed one
        // (otherwise there's no fraud)
        if proof.correct_post_state_root == proof.claimed_post_state_root {
            return FraudProofResult::Invalid {
                reason: "correct state root matches claimed — no fraud".into(),
            };
        }

        // Re-execute the disputed transaction from the pre-state and verify
        // that the result matches the challenger's claimed correct_post_state_root.
        // This is the critical check that prevents a challenger from fabricating
        // a fraud proof by supplying an arbitrary correct_post_state_root.
        let computed_post_state = match executor.re_execute(&proof.pre_state_root, &proof.tx_data) {
            Ok(root) => root,
            Err(reason) => {
                return FraudProofResult::Invalid {
                    reason: format!("re-execution failed: {}", reason),
                }
            }
        };

        if computed_post_state != proof.correct_post_state_root {
            return FraudProofResult::Invalid {
                reason: "re-executed post-state root does not match challenger's claim".into(),
            };
        }

        // Re-execution confirms the sequencer's post_state_root was wrong.
        let reward = sequencer_bond.saturating_mul(self.challenger_reward_pct as u128) / 100;

        FraudProofResult::Valid {
            slashed_sequencer: *sequencer,
            challenger_reward: reward,
        }
    }
}
