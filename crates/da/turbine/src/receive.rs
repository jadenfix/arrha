use std::collections::{HashMap, VecDeque};

use aether_da_erasure::ReedSolomonDecoder;
use aether_da_shreds::Shred;
use aether_types::H256;
use anyhow::{bail, Result};

/// Maximum number of in-flight blocks to prevent memory exhaustion DoS.
const MAX_PENDING_BLOCKS: usize = 64;

/// Maximum aggregate bytes of pending shred payloads (128 MiB).
/// Bounds total memory a malicious peer can force the receiver to hold.
const MAX_PENDING_BYTES: usize = 128 * 1024 * 1024;

pub struct TurbineReceiver {
    decoder: ReedSolomonDecoder,
    pending: HashMap<H256, Vec<Option<Vec<u8>>>>,
    pending_order: VecDeque<H256>,
    pending_bytes: usize,
}

impl TurbineReceiver {
    pub fn new(data_shards: usize, parity_shards: usize) -> Result<Self> {
        Ok(TurbineReceiver {
            decoder: ReedSolomonDecoder::new(data_shards, parity_shards)?,
            pending: HashMap::new(),
            pending_order: VecDeque::new(),
            pending_bytes: 0,
        })
    }

    fn block_bytes(shards: &[Option<Vec<u8>>]) -> usize {
        shards
            .iter()
            .map(|s| s.as_ref().map_or(0, |v| v.len()))
            .sum()
    }

    fn evict_oldest_pending(&mut self) {
        if let Some(block_id) = self.pending_order.pop_front() {
            if let Some(shards) = self.pending.remove(&block_id) {
                self.pending_bytes = self
                    .pending_bytes
                    .saturating_sub(Self::block_bytes(&shards));
            }
        }
    }

    fn remove_pending(&mut self, block_id: &H256) {
        if let Some(shards) = self.pending.remove(block_id) {
            self.pending_bytes = self
                .pending_bytes
                .saturating_sub(Self::block_bytes(&shards));
        }
        self.pending_order.retain(|queued| queued != block_id);
    }

    pub fn ingest_shred(&mut self, shred: Shred) -> Result<Option<Vec<u8>>> {
        let (data_shards, parity_shards) = self.decoder.shard_config();
        let total_shards = data_shards + parity_shards;
        let shred_idx = shred.index as usize;
        if shred_idx >= total_shards {
            bail!(
                "shred index {} exceeds shard count {}",
                shred.index,
                total_shards
            );
        }

        let payload_len = shred.payload.len();

        if self.pending_bytes.saturating_add(payload_len) > MAX_PENDING_BYTES {
            bail!(
                "pending data limit exceeded ({} + {} > {})",
                self.pending_bytes,
                payload_len,
                MAX_PENDING_BYTES
            );
        }

        let is_new_block = !self.pending.contains_key(&shred.block_id);

        if is_new_block && self.pending.len() >= MAX_PENDING_BLOCKS {
            self.evict_oldest_pending();
        }

        let entry = self
            .pending
            .entry(shred.block_id)
            .or_insert_with(|| vec![None; total_shards]);

        if is_new_block {
            self.pending_order.push_back(shred.block_id);
        }

        if let Some(old) = entry[shred_idx].take() {
            self.pending_bytes = self.pending_bytes.saturating_sub(old.len());
        }
        entry[shred_idx] = Some(shred.payload.clone());
        self.pending_bytes = self.pending_bytes.saturating_add(payload_len);

        if entry.iter().filter(|chunk| chunk.is_some()).count() < data_shards {
            return Ok(None);
        }

        let recovered = self.decoder.decode(entry)?;
        self.remove_pending(&shred.block_id);
        Ok(Some(recovered))
    }
}
