use crate::primitives::{Address, PublicKey, Signature, Slot, H256};
use crate::transaction::Transaction;
use serde::{Deserialize, Serialize};

/// A signed vote included as part of slash evidence.
///
/// Contains enough information to cryptographically verify the vote
/// was actually cast by the claimed validator.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SlashVote {
    pub slot: u64,
    pub block_hash: H256,
    pub validator: Address,
    pub validator_pubkey: PublicKey,
    pub signature: Signature,
}

/// The type of misbehavior being reported.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SlashEvidenceType {
    /// Same slot, different blocks.
    DoubleSign,
    /// Vote A's range strictly contains Vote B's range.
    SurroundVote,
}

/// Evidence of validator misbehavior included in a block.
///
/// Included by the block proposer; processed during block validation to
/// reduce the offending validator's stake.  Each entry MUST carry two
/// cryptographically signed conflicting votes.  The node verifies the
/// BLS signatures before applying the slash — without valid proof the
/// evidence is silently skipped.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SlashEvidence {
    /// Address of the validator being slashed.
    pub validator: Address,
    /// Slash rate in basis points (e.g. 500 = 5%).  Ignored by the
    /// verifier — the actual rate is determined by `evidence_type`.
    pub slash_rate_bps: u32,
    /// Human-readable reason tag (e.g. "double_sign", "surround_vote").
    pub reason: String,
    /// The two conflicting votes that prove the misbehavior.
    #[serde(default)]
    pub vote1: Option<SlashVote>,
    #[serde(default)]
    pub vote2: Option<SlashVote>,
    /// The type of misbehavior.
    #[serde(default)]
    pub evidence_type: Option<SlashEvidenceType>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Block {
    pub header: BlockHeader,
    pub transactions: Vec<Transaction>,
    pub aggregated_vote: Option<AggregatedVote>,
    /// Slash evidence included by the proposer.  Defaults to empty so
    /// existing serialized blocks deserialize without error.
    #[serde(default)]
    pub slash_evidence: Vec<SlashEvidence>,
}

/// Current protocol version. Incremented on hard forks.
pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockHeader {
    /// Protocol version (for hard fork signaling).
    pub version: u32,
    pub slot: Slot,
    pub parent_hash: H256,
    pub state_root: H256,
    pub transactions_root: H256,
    pub receipts_root: H256,
    pub proposer: Address,
    pub vrf_proof: VrfProof,
    pub timestamp: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VrfProof {
    pub output: [u8; 32],
    pub proof: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AggregatedVote {
    pub slot: Slot,
    pub block_hash: H256,
    pub aggregated_signature: Vec<u8>,
    pub signers: Vec<PublicKey>,
    pub total_stake: u128,
}

impl Block {
    pub fn hash(&self) -> H256 {
        use sha2::{Digest, Sha256};
        // Evidence changes branch-local staking state, so it must be part of
        // the certified block identity. Keep the QC out of the hash: QCs
        // certify a block and are attached after its ID is established.
        // The domain tag makes this commitment distinct from the v1
        // header-only block ID and from other serialized protocol objects.
        const DOMAIN: &[u8] = b"AETHER_BLOCK_V2\0";
        let header = bincode::serialize(&self.header).expect("header serialization infallible");
        let evidence = bincode::serialize(&self.slash_evidence)
            .expect("slash evidence serialization infallible");
        let mut hasher = Sha256::new();
        hasher.update(DOMAIN);
        hasher.update((header.len() as u64).to_le_bytes());
        hasher.update(header);
        hasher.update((evidence.len() as u64).to_le_bytes());
        hasher.update(evidence);
        let hash = hasher.finalize();
        H256::from_slice(&hash).expect("SHA256 produces 32 bytes")
    }

    pub fn new(
        slot: Slot,
        parent_hash: H256,
        proposer: Address,
        vrf_proof: VrfProof,
        transactions: Vec<Transaction>,
    ) -> Self {
        Block {
            header: BlockHeader {
                version: PROTOCOL_VERSION,
                slot,
                parent_hash,
                state_root: H256::zero(),
                transactions_root: H256::zero(),
                receipts_root: H256::zero(),
                proposer,
                vrf_proof,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            },
            transactions,
            aggregated_vote: None,
            slash_evidence: Vec::new(),
        }
    }
}
