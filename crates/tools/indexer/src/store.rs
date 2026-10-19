use aether_types::{Block, H256};
use anyhow::{Context, Result};
use rocksdb::{ColumnFamilyDescriptor, Options, WriteBatch, DB};
use serde::{Deserialize, Serialize};
use std::path::Path;

const CF_BLOCKS: &str = "indexed_blocks";
const CF_TX_INDEX: &str = "tx_to_slot";
const CF_META: &str = "indexer_meta";

/// Indexed block record stored in RocksDB.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexedBlock {
    pub slot: u64,
    pub hash: H256,
    pub tx_count: usize,
    pub proposer: String,
    pub timestamp: u64,
    pub tx_hashes: Vec<H256>,
}

/// Persistent indexer store backed by RocksDB.
pub struct PersistentStore {
    db: DB,
}

impl PersistentStore {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);

        let cfs = vec![
            ColumnFamilyDescriptor::new(CF_BLOCKS, Options::default()),
            ColumnFamilyDescriptor::new(CF_TX_INDEX, Options::default()),
            ColumnFamilyDescriptor::new(CF_META, Options::default()),
        ];

        let db =
            DB::open_cf_descriptors(&opts, path, cfs).context("failed to open indexer database")?;

        Ok(PersistentStore { db })
    }

    /// Ingest a block into the index.
    ///
    /// All writes (block record, tx→slot index, latest_slot) are committed in a
    /// single atomic WriteBatch so a crash mid-ingest cannot leave partial state.
    pub fn ingest(&self, block: &Block) -> Result<()> {
        let indexed = IndexedBlock {
            slot: block.header.slot,
            hash: block.hash(),
            tx_count: block.transactions.len(),
            proposer: format!("{:?}", block.header.proposer),
            timestamp: block.header.timestamp,
            tx_hashes: block.transactions.iter().map(|tx| tx.hash()).collect(),
        };

        let key = block.header.slot.to_be_bytes();
        let value = bincode::serialize(&indexed)?;

        let mut batch = WriteBatch::default();

        let cf = self.db.cf_handle(CF_BLOCKS).context("missing blocks CF")?;
        batch.put_cf(cf, key, &value);

        // Index each transaction hash → slot
        let tx_cf = self
            .db
            .cf_handle(CF_TX_INDEX)
            .context("missing tx_index CF")?;
        for tx in &block.transactions {
            let tx_hash = tx.hash();
            batch.put_cf(tx_cf, tx_hash.as_bytes(), key);
        }

        // Update latest slot
        let meta_cf = self.db.cf_handle(CF_META).context("missing meta CF")?;
        batch.put_cf(meta_cf, b"latest_slot", key);

        // Atomic commit — all-or-nothing
        self.db.write(batch)?;

        Ok(())
    }

    /// Get an indexed block by slot number.
    pub fn get_block(&self, slot: u64) -> Result<Option<IndexedBlock>> {
        let cf = self.db.cf_handle(CF_BLOCKS).context("missing blocks CF")?;
        let key = slot.to_be_bytes();
        match self.db.get_cf(cf, key)? {
            Some(bytes) => Ok(Some(bincode::deserialize(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Look up which slot a transaction belongs to.
    pub fn get_tx_slot(&self, tx_hash: &H256) -> Result<Option<u64>> {
        let cf = self
            .db
            .cf_handle(CF_TX_INDEX)
            .context("missing tx_index CF")?;
        match self.db.get_cf(cf, tx_hash.as_bytes())? {
            Some(bytes) => {
                let slot_bytes: [u8; 8] =
                    bytes.as_slice().try_into().context("invalid slot bytes")?;
                Ok(Some(u64::from_be_bytes(slot_bytes)))
            }
            None => Ok(None),
        }
    }

    /// Get the latest indexed slot.
    pub fn latest_slot(&self) -> Result<u64> {
        let cf = self.db.cf_handle(CF_META).context("missing meta CF")?;
        match self.db.get_cf(cf, b"latest_slot")? {
            Some(bytes) => {
                let slot_bytes: [u8; 8] =
                    bytes.as_slice().try_into().context("invalid slot bytes")?;
                Ok(u64::from_be_bytes(slot_bytes))
            }
            None => Ok(0),
        }
    }

    /// Count total indexed blocks (approximate — scans CF).
    pub fn block_count(&self) -> Result<usize> {
        let cf = self.db.cf_handle(CF_BLOCKS).context("missing blocks CF")?;
        let count = self
            .db
            .iterator_cf(cf, rocksdb::IteratorMode::Start)
            .count();
        Ok(count)
    }
}
