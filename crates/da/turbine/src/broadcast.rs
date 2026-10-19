use aether_crypto_primitives::Keypair;
use aether_da_erasure::ReedSolomonEncoder;
use aether_da_shreds::{shred::ShredVariant, Shred};
use aether_types::{Signature, Slot, H256};
use anyhow::Result;

pub struct TurbineBroadcaster {
    encoder: ReedSolomonEncoder,
    protocol_version: u16,
    /// Ed25519 keypair used to sign shreds, proving proposer authenticity.
    signing_key: Keypair,
}

impl TurbineBroadcaster {
    pub fn new(
        data_shards: usize,
        parity_shards: usize,
        protocol_version: u16,
        signing_key: Keypair,
    ) -> Result<Self> {
        Ok(TurbineBroadcaster {
            encoder: ReedSolomonEncoder::new(data_shards, parity_shards)?,
            protocol_version,
            signing_key,
        })
    }

    pub fn shard_count(&self) -> usize {
        self.encoder.data_shards + self.encoder.parity_shards
    }

    pub fn make_shreds(&self, slot: Slot, block_id: H256, payload: &[u8]) -> Result<Vec<Shred>> {
        let shards = self.encoder.encode(payload)?;
        let mut result = Vec::with_capacity(shards.len());

        for (idx, chunk) in shards.into_iter().enumerate() {
            let shard_index = u32::try_from(idx)
                .map_err(|_| anyhow::anyhow!("shard index {idx} exceeds u32::MAX"))?;

            let variant = if idx < self.encoder.data_shards {
                ShredVariant::Data
            } else {
                ShredVariant::Parity
            };

            let payload_hash = Shred::hash_payload(&chunk);
            let msg = Shred::build_signing_message(slot, shard_index, &payload_hash);
            let signature = Signature::from_bytes(self.signing_key.sign(&msg));

            result.push(Shred::new(
                variant,
                slot,
                shard_index,
                self.protocol_version,
                0,
                block_id,
                chunk,
                signature,
            ));
        }

        Ok(result)
    }

    /// Returns the public key bytes for this broadcaster's signing key.
    pub fn public_key(&self) -> Vec<u8> {
        self.signing_key.public_key()
    }
}
