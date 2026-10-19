use aether_types::{Block, H256};
use anyhow::{Context, Result};
use rocksdb::{ColumnFamilyDescriptor, Options, WriteBatch, DB};
use serde::{Deserialize, Serialize};
use std::path::Path;

const CF_BLOCKS: &str = "indexed_blocks";
const CF_TX_INDEX: &str = "tx_to_slot";
const CF_META: &str = "indexer_meta";
const META_CHAIN_IDENTITY: &[u8] = b"chain_identity";

/// RPC identity pinned to this persistent index. The genesis hash and full
/// chain fingerprint distinguish networks that reuse a numeric chain ID.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChainIdentity {
    pub chain_id: u64,
    pub genesis_hash: String,
    pub chain_identity: String,
}

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

    /// Pin an empty index to a chain or verify the identity already stored.
    /// Existing block data without identity metadata is rejected because its
    /// source chain cannot be established safely.
    pub fn ensure_chain_identity(&self, identity: &ChainIdentity) -> Result<()> {
        let cf = self.db.cf_handle(CF_META).context("missing meta CF")?;
        match self.db.get_cf(cf, META_CHAIN_IDENTITY)? {
            Some(bytes) => {
                let stored: ChainIdentity =
                    bincode::deserialize(&bytes).context("invalid stored chain identity")?;
                if stored != *identity {
                    anyhow::bail!(
                        "index database belongs to chain_id={} genesis_hash={} chain_identity={}, RPC reports chain_id={} genesis_hash={} chain_identity={}",
                        stored.chain_id,
                        stored.genesis_hash,
                        stored.chain_identity,
                        identity.chain_id,
                        identity.genesis_hash,
                        identity.chain_identity
                    );
                }
            }
            None => {
                if self.db.get_cf(cf, b"latest_slot")?.is_some() || self.block_count()? > 0 {
                    anyhow::bail!(
                        "index database contains blocks but has no chain identity; configure a fresh INDEXER_DB_PATH to reindex safely"
                    );
                }
                self.db
                    .put_cf(cf, META_CHAIN_IDENTITY, bincode::serialize(identity)?)?;
            }
        }
        Ok(())
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

        let blocks_cf = self.db.cf_handle(CF_BLOCKS).context("missing blocks CF")?;
        if let Some(bytes) = self.db.get_cf(blocks_cf, key)? {
            let existing: IndexedBlock =
                bincode::deserialize(&bytes).context("invalid stored block record")?;
            if existing.hash != indexed.hash {
                anyhow::bail!(
                    "refusing to replace finalized slot {} with a different block hash (stored {:?}, received {:?})",
                    block.header.slot,
                    existing.hash,
                    indexed.hash
                );
            }
            return Ok(());
        }

        let mut batch = WriteBatch::default();

        batch.put_cf(blocks_cf, key, &value);

        // Index each transaction hash → slot
        let tx_cf = self
            .db
            .cf_handle(CF_TX_INDEX)
            .context("missing tx_index CF")?;
        for tx in &block.transactions {
            let tx_hash = tx.hash();
            if let Some(existing_slot) = self.db.get_cf(tx_cf, tx_hash.as_bytes())? {
                let slot_bytes: [u8; 8] = existing_slot
                    .as_slice()
                    .try_into()
                    .context("invalid transaction index slot bytes")?;
                if u64::from_be_bytes(slot_bytes) != block.header.slot {
                    anyhow::bail!(
                        "transaction {:?} is already indexed at a different finalized slot",
                        tx_hash
                    );
                }
            }
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

    /// Read the most recent persisted block as the restart/reconnect checkpoint.
    pub fn checkpoint(&self) -> Result<Option<IndexedBlock>> {
        let cf = self.db.cf_handle(CF_META).context("missing meta CF")?;
        let Some(bytes) = self.db.get_cf(cf, b"latest_slot")? else {
            if self.block_count()? == 0 {
                return Ok(None);
            }
            anyhow::bail!("index database has blocks but no checkpoint metadata");
        };
        let slot_bytes: [u8; 8] = bytes
            .as_slice()
            .try_into()
            .context("invalid latest slot bytes")?;
        let slot = u64::from_be_bytes(slot_bytes);
        self.get_block(slot)?.map(Some).ok_or_else(|| {
            anyhow::anyhow!("index checkpoint metadata points to missing slot {slot}")
        })
    }

    /// Count indexed blocks, propagating any RocksDB iterator errors.
    pub fn block_count(&self) -> Result<usize> {
        let cf = self.db.cf_handle(CF_BLOCKS).context("missing blocks CF")?;
        self.db
            .iterator_cf(cf, rocksdb::IteratorMode::Start)
            .try_fold(0usize, |count, entry| {
                entry.context("iterate indexed blocks")?;
                count.checked_add(1).context("indexed block count overflow")
            })
    }
}
