use aether_types::{Block, BlockHeader, Transaction, H256};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Compact block representation for bandwidth-efficient propagation.
///
/// Instead of sending full blocks (which can be 2MB with 5000 txs),
/// sends only the header + transaction hashes. Peers reconstruct
/// the full block from their mempool.
///
/// Bandwidth savings: ~90% for validators with warm mempools.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompactBlock {
    pub header: BlockHeader,
    pub tx_hashes: Vec<H256>,
}

/// Result of attempting to reconstruct a full block from a compact block.
pub struct ReconstructionResult {
    /// The reconstructed block (if all txs were available).
    pub block: Option<Block>,
    /// Transaction hashes that are missing from the local mempool.
    pub missing: Vec<H256>,
}

impl CompactBlock {
    /// Create a compact block from a full block.
    pub fn from_block(block: &Block) -> Self {
        CompactBlock {
            header: block.header.clone(),
            tx_hashes: block.transactions.iter().map(|tx| tx.hash()).collect(),
        }
    }

    /// Attempt to reconstruct the full block using locally available transactions.
    pub fn reconstruct(&self, known_txs: &HashMap<H256, Transaction>) -> ReconstructionResult {
        let mut transactions = Vec::with_capacity(self.tx_hashes.len());
        let mut missing = Vec::new();

        for hash in &self.tx_hashes {
            match known_txs.get(hash) {
                Some(tx) => transactions.push(tx.clone()),
                None => missing.push(*hash),
            }
        }

        if missing.is_empty() {
            ReconstructionResult {
                block: Some(Block {
                    header: self.header.clone(),
                    transactions,
                    aggregated_vote: None,
                    slash_evidence: Vec::new(),
                }),
                missing: vec![],
            }
        } else {
            ReconstructionResult {
                block: None,
                missing,
            }
        }
    }

    /// Serialized size of this compact block.
    pub fn wire_size(&self) -> usize {
        bincode::serialize(self).map(|b| b.len()).unwrap_or(0)
    }
}

/// Compress data with zstd for P2P messages > 256 bytes.
pub fn compress_message(data: &[u8]) -> Vec<u8> {
    if data.len() <= 256 {
        // Prepend 0x00 = uncompressed
        let mut out = Vec::with_capacity(1 + data.len());
        out.push(0x00);
        out.extend_from_slice(data);
        return out;
    }

    // Prepend 0x01 = zstd compressed
    match zstd::encode_all(data, 1) {
        Ok(compressed) => {
            let mut out = Vec::with_capacity(1 + compressed.len());
            out.push(0x01);
            out.extend_from_slice(&compressed);
            out
        }
        Err(_) => {
            // Fallback to uncompressed
            let mut out = Vec::with_capacity(1 + data.len());
            out.push(0x00);
            out.extend_from_slice(data);
            out
        }
    }
}

/// Maximum decompressed message size (16 MB) to prevent decompression bombs.
const MAX_DECOMPRESSED_SIZE: usize = 16 * 1024 * 1024;

/// Decompress a P2P message.
pub fn decompress_message(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    if data.is_empty() {
        anyhow::bail!("empty message");
    }

    match data[0] {
        0x00 => {
            if data.len() - 1 > MAX_DECOMPRESSED_SIZE {
                anyhow::bail!(
                    "uncompressed message too large: {} bytes (max {})",
                    data.len() - 1,
                    MAX_DECOMPRESSED_SIZE
                );
            }
            Ok(data[1..].to_vec())
        }
        0x01 => {
            let decompressed = zstd::decode_all(&data[1..])?;
            if decompressed.len() > MAX_DECOMPRESSED_SIZE {
                anyhow::bail!(
                    "decompressed message too large: {} bytes (max {})",
                    decompressed.len(),
                    MAX_DECOMPRESSED_SIZE
                );
            }
            Ok(decompressed)
        }
        tag => anyhow::bail!("unknown compression tag: {}", tag),
    }
}
