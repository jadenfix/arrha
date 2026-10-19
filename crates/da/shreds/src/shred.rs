use aether_types::{Signature, Slot, H256};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ShredVariant {
    Data,
    Parity,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Shred {
    pub variant: ShredVariant,
    pub slot: Slot,
    pub index: u32,
    pub version: u16,
    pub fec_set_index: u32,
    pub block_id: H256,
    pub payload: Vec<u8>,
    pub signature: Signature,
    pub payload_hash: H256,
}

impl Shred {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        variant: ShredVariant,
        slot: Slot,
        index: u32,
        version: u16,
        fec_set_index: u32,
        block_id: H256,
        payload: Vec<u8>,
        signature: Signature,
    ) -> Self {
        let payload_hash = Self::hash_payload(&payload);
        Shred {
            variant,
            slot,
            index,
            version,
            fec_set_index,
            block_id,
            payload,
            signature,
            payload_hash,
        }
    }

    pub fn hash_payload(payload: &[u8]) -> H256 {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(payload);
        H256::from(<[u8; 32]>::from(hasher.finalize()))
    }

    /// Canonical message used for Ed25519 signing and verification.
    /// Includes slot, index, and payload hash to bind the signature
    /// to a specific shred without including the full payload.
    pub fn signing_message(&self) -> Vec<u8> {
        let mut msg = Vec::with_capacity(8 + 4 + 32);
        msg.extend_from_slice(&self.slot.to_le_bytes());
        msg.extend_from_slice(&self.index.to_le_bytes());
        msg.extend_from_slice(self.payload_hash.as_bytes());
        msg
    }

    /// Build the signing message from components (for use before Shred construction).
    pub fn build_signing_message(slot: Slot, index: u32, payload_hash: &H256) -> Vec<u8> {
        let mut msg = Vec::with_capacity(8 + 4 + 32);
        msg.extend_from_slice(&slot.to_le_bytes());
        msg.extend_from_slice(&index.to_le_bytes());
        msg.extend_from_slice(payload_hash.as_bytes());
        msg
    }
}
