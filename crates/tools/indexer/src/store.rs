use aether_sdk::types::RpcReceipt;
use aether_types::{Block, H256};
use anyhow::{Context, Result};
use rocksdb::{ColumnFamilyDescriptor, Options, WriteBatch, DB};
use serde::{Deserialize, Serialize};
use std::path::Path;

const CF_BLOCKS: &str = "indexed_blocks";
const CF_TX_INDEX: &str = "tx_to_slot";
const CF_META: &str = "indexer_meta";
const CF_RECEIPTS: &str = "indexed_receipts";

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
            ColumnFamilyDescriptor::new(CF_RECEIPTS, Options::default()),
        ];

        let db =
            DB::open_cf_descriptors(&opts, path, cfs).context("failed to open indexer database")?;

        Ok(PersistentStore { db })
    }

    /// Pin this database to the configured chain; never silently reuse another chain's cursor.
    pub fn bind_chain(&self, chain_id: u64) -> Result<()> {
        let cf = self.db.cf_handle(CF_META).context("missing meta CF")?;
        let key = chain_id.to_be_bytes();
        if let Some(existing) = self.db.get_cf(cf, b"chain_id")? {
            anyhow::ensure!(
                existing.as_slice() == key,
                "indexer database belongs to another chain"
            );
        } else {
            self.db.put_cf(cf, b"chain_id", key)?;
        }
        Ok(())
    }

    pub fn next_slot(&self) -> Result<u64> {
        let cf = self.db.cf_handle(CF_META).context("missing meta CF")?;
        self.db
            .get_cf(cf, b"next_slot")?
            .map(|bytes| {
                let raw: [u8; 8] = bytes.as_slice().try_into().context("invalid cursor")?;
                Ok(u64::from_be_bytes(raw))
            })
            .unwrap_or(Ok(0))
    }

    /// Commit the complete finalized slot and its receipts together with the cursor.
    /// Missing receipts, mismatched hashes and out-of-order slots cannot advance it.
    pub fn commit_finalized_slot(
        &self,
        slot: u64,
        block: Option<&Block>,
        receipts: &[RpcReceipt],
    ) -> Result<()> {
        anyhow::ensure!(slot == self.next_slot()?, "out-of-order finalized slot");
        let mut batch = WriteBatch::default();
        let meta = self.db.cf_handle(CF_META).context("missing meta CF")?;
        if let Some(block) = block {
            anyhow::ensure!(block.header.slot == slot, "block slot mismatch");
            anyhow::ensure!(
                receipts.len() == block.transactions.len(),
                "incomplete receipts"
            );
            let hash = block.hash();
            let tx_hashes: Vec<H256> = block.transactions.iter().map(|tx| tx.hash()).collect();
            let indexed = IndexedBlock {
                slot,
                hash,
                tx_count: tx_hashes.len(),
                proposer: format!("{:?}", block.header.proposer),
                timestamp: block.header.timestamp,
                tx_hashes: tx_hashes.clone(),
            };
            let blocks_cf = self.db.cf_handle(CF_BLOCKS).context("missing blocks CF")?;
            let tx_cf = self.db.cf_handle(CF_TX_INDEX).context("missing tx CF")?;
            let receipts_cf = self
                .db
                .cf_handle(CF_RECEIPTS)
                .context("missing receipts CF")?;
            batch.put_cf(blocks_cf, slot.to_be_bytes(), bincode::serialize(&indexed)?);
            for (expected, receipt) in tx_hashes.iter().zip(receipts) {
                anyhow::ensure!(
                    receipt.tx_hash == *expected
                        && receipt.block_hash == hash
                        && receipt.slot == slot,
                    "receipt identity mismatch"
                );
                batch.put_cf(tx_cf, expected.as_bytes(), slot.to_be_bytes());
                batch.put_cf(
                    receipts_cf,
                    expected.as_bytes(),
                    serde_json::to_vec(receipt)?,
                );
            }
            batch.put_cf(meta, b"latest_slot", slot.to_be_bytes());
        } else {
            anyhow::ensure!(receipts.is_empty(), "receipts without block");
        }
        let next = slot.checked_add(1).context("slot overflow")?;
        batch.put_cf(meta, b"next_slot", next.to_be_bytes());
        self.db.write(batch)?;
        self.db.flush()?;
        Ok(())
    }

    pub fn get_receipt(&self, hash: &H256) -> Result<Option<RpcReceipt>> {
        let cf = self
            .db
            .cf_handle(CF_RECEIPTS)
            .context("missing receipts CF")?;
        self.db
            .get_cf(cf, hash.as_bytes())?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
            .transpose()
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

#[cfg(test)]
mod tests {
    use super::*;
    use aether_types::*;

    fn make_block(slot: u64, num_txs: usize) -> Block {
        let txs: Vec<Transaction> = (0..num_txs)
            .map(|i| Transaction {
                nonce: i as u64,
                chain_id: 1,
                sender: Address::from_slice(&[1u8; 20]).unwrap(),
                sender_pubkey: PublicKey::from_bytes(vec![2u8; 32]),
                inputs: vec![],
                outputs: vec![],
                reads: std::collections::HashSet::new(),
                writes: std::collections::HashSet::new(),
                program_id: None,
                data: vec![slot as u8, i as u8],
                gas_limit: 21000,
                fee: 1000,
                signature: Signature::from_bytes(vec![3u8; 64]),
            })
            .collect();

        Block::new(
            slot,
            H256::zero(),
            Address::from_slice(&[1u8; 20]).unwrap(),
            VrfProof {
                output: [0u8; 32],
                proof: vec![0u8; 80],
            },
            txs,
        )
    }

    #[test]
    fn finalized_commit_is_atomic_and_durable() {
        let dir = tempfile::TempDir::new().unwrap();
        let block = make_block(0, 1);
        let receipt = RpcReceipt {
            tx_hash: block.transactions[0].hash(),
            block_hash: block.hash(),
            slot: 0,
            status: serde_json::json!("Success"),
            gas_used: 21000,
        };
        {
            let store = PersistentStore::open(dir.path()).unwrap();
            store.bind_chain(900).unwrap();
            assert!(store.bind_chain(1).is_err());
            assert!(store.commit_finalized_slot(0, Some(&block), &[]).is_err());
            let mut bad = receipt.clone();
            bad.block_hash = H256::zero();
            assert!(store
                .commit_finalized_slot(0, Some(&block), &[bad])
                .is_err());
            assert_eq!(store.next_slot().unwrap(), 0);
            assert!(store.get_block(0).unwrap().is_none());
            assert!(store.get_receipt(&receipt.tx_hash).unwrap().is_none());
            store
                .commit_finalized_slot(0, Some(&block), &[receipt.clone()])
                .unwrap();
            assert!(store
                .commit_finalized_slot(0, Some(&block), &[receipt.clone()])
                .is_err());
        }
        let store = PersistentStore::open(dir.path()).unwrap();
        assert_eq!(store.next_slot().unwrap(), 1);
        assert_eq!(
            store
                .get_receipt(&receipt.tx_hash)
                .unwrap()
                .unwrap()
                .block_hash,
            block.hash()
        );
        assert_eq!(store.get_tx_slot(&receipt.tx_hash).unwrap(), Some(0));
        store.commit_finalized_slot(1, None, &[]).unwrap();
        assert_eq!(store.next_slot().unwrap(), 2);
        assert_eq!(store.block_count().unwrap(), 1);
    }

    #[test]
    fn test_ingest_and_query() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = PersistentStore::open(dir.path()).unwrap();

        let block = make_block(42, 3);
        store.ingest(&block).unwrap();

        let indexed = store.get_block(42).unwrap().unwrap();
        assert_eq!(indexed.slot, 42);
        assert_eq!(indexed.tx_count, 3);
    }

    #[test]
    fn test_tx_lookup() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = PersistentStore::open(dir.path()).unwrap();

        let block = make_block(10, 2);
        let tx_hash = block.transactions[0].hash();
        store.ingest(&block).unwrap();

        let slot = store.get_tx_slot(&tx_hash).unwrap();
        assert_eq!(slot, Some(10));
    }

    #[test]
    fn test_latest_slot_tracking() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = PersistentStore::open(dir.path()).unwrap();

        assert_eq!(store.latest_slot().unwrap(), 0);

        store.ingest(&make_block(5, 1)).unwrap();
        assert_eq!(store.latest_slot().unwrap(), 5);

        store.ingest(&make_block(10, 1)).unwrap();
        assert_eq!(store.latest_slot().unwrap(), 10);
    }

    #[test]
    fn test_persistence_across_reopen() {
        let dir = tempfile::TempDir::new().unwrap();

        {
            let store = PersistentStore::open(dir.path()).unwrap();
            store.ingest(&make_block(100, 5)).unwrap();
        }

        // Reopen and verify data persists
        let store = PersistentStore::open(dir.path()).unwrap();
        let block = store.get_block(100).unwrap().unwrap();
        assert_eq!(block.slot, 100);
        assert_eq!(block.tx_count, 5);
        assert_eq!(store.latest_slot().unwrap(), 100);
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use aether_types::*;
    use proptest::prelude::*;

    fn make_block_arb(slot: u64, num_txs: usize) -> Block {
        let txs: Vec<Transaction> = (0..num_txs)
            .map(|i| Transaction {
                nonce: i as u64,
                chain_id: 1,
                sender: Address::from_slice(&[((slot & 0xff) as u8).wrapping_add(1); 20]).unwrap(),
                sender_pubkey: PublicKey::from_bytes(vec![2u8; 32]),
                inputs: vec![],
                outputs: vec![],
                reads: std::collections::HashSet::new(),
                writes: std::collections::HashSet::new(),
                program_id: None,
                data: vec![slot as u8, i as u8],
                gas_limit: 21000,
                fee: 1000,
                signature: Signature::from_bytes(vec![3u8; 64]),
            })
            .collect();

        Block::new(
            slot,
            H256::zero(),
            Address::from_slice(&[1u8; 20]).unwrap(),
            VrfProof {
                output: [0u8; 32],
                proof: vec![0u8; 80],
            },
            txs,
        )
    }

    proptest! {
        /// Ingesting a block and querying it back returns the same slot and tx count.
        #[test]
        fn prop_ingest_roundtrip(slot in 1u64..=10_000u64, num_txs in 0usize..=10) {
            let dir = tempfile::TempDir::new().unwrap();
            let store = PersistentStore::open(dir.path()).unwrap();
            let block = make_block_arb(slot, num_txs);
            store.ingest(&block).unwrap();

            let indexed = store.get_block(slot).unwrap().unwrap();
            prop_assert_eq!(indexed.slot, slot);
            prop_assert_eq!(indexed.tx_count, num_txs);
            prop_assert_eq!(indexed.tx_hashes.len(), num_txs);
        }

        /// latest_slot reflects the most recently ingested slot (last-write-wins).
        #[test]
        fn prop_latest_slot_is_last_ingested(slots in prop::collection::vec(1u64..=50_000u64, 1..=8)) {
            let dir = tempfile::TempDir::new().unwrap();
            let store = PersistentStore::open(dir.path()).unwrap();

            let mut last_slot = 0u64;
            for slot in &slots {
                store.ingest(&make_block_arb(*slot, 1)).unwrap();
                last_slot = *slot;
            }
            prop_assert_eq!(store.latest_slot().unwrap(), last_slot);
        }

        /// When slots are ingested in ascending order, latest_slot equals the max.
        #[test]
        fn prop_ascending_ingest_latest_slot_is_max(
            mut slots in prop::collection::vec(1u64..=50_000u64, 1..=8)
        ) {
            slots.sort_unstable();
            slots.dedup();
            let dir = tempfile::TempDir::new().unwrap();
            let store = PersistentStore::open(dir.path()).unwrap();

            let max_slot = *slots.last().unwrap();
            for slot in &slots {
                store.ingest(&make_block_arb(*slot, 1)).unwrap();
            }
            prop_assert_eq!(store.latest_slot().unwrap(), max_slot);
        }

        /// block_count equals the number of unique slots ingested.
        #[test]
        fn prop_block_count_equals_unique_slots(
            slots in prop::collection::vec(1u64..=100u64, 1..=10)
        ) {
            let dir = tempfile::TempDir::new().unwrap();
            let store = PersistentStore::open(dir.path()).unwrap();

            let mut seen = std::collections::HashSet::new();
            for slot in &slots {
                if seen.insert(*slot) {
                    store.ingest(&make_block_arb(*slot, 1)).unwrap();
                }
            }
            prop_assert_eq!(store.block_count().unwrap(), seen.len());
        }

        /// Every transaction hash in an ingested block resolves to its block's slot.
        #[test]
        fn prop_tx_lookup_resolves_to_correct_slot(
            slot in 1u64..=10_000u64,
            num_txs in 1usize..=8
        ) {
            let dir = tempfile::TempDir::new().unwrap();
            let store = PersistentStore::open(dir.path()).unwrap();
            let block = make_block_arb(slot, num_txs);
            let tx_hashes: Vec<H256> = block.transactions.iter().map(|tx| tx.hash()).collect();
            store.ingest(&block).unwrap();

            for hash in &tx_hashes {
                let resolved = store.get_tx_slot(hash).unwrap();
                prop_assert_eq!(resolved, Some(slot));
            }
        }

        /// Querying a slot that was never ingested returns None.
        #[test]
        fn prop_missing_slot_returns_none(slot in 1u64..=10_000u64) {
            let dir = tempfile::TempDir::new().unwrap();
            let store = PersistentStore::open(dir.path()).unwrap();
            // Ingest a *different* slot to ensure the store isn't empty
            let other_slot = if slot == 1 { 2 } else { slot - 1 };
            store.ingest(&make_block_arb(other_slot, 1)).unwrap();

            prop_assert!(store.get_block(slot).unwrap().is_none());
        }

        /// IndexedBlock proposer field is always a non-empty string.
        #[test]
        fn prop_indexed_block_proposer_nonempty(slot in 1u64..=10_000u64) {
            let dir = tempfile::TempDir::new().unwrap();
            let store = PersistentStore::open(dir.path()).unwrap();
            store.ingest(&make_block_arb(slot, 1)).unwrap();
            let indexed = store.get_block(slot).unwrap().unwrap();
            prop_assert!(!indexed.proposer.is_empty());
        }

        /// Re-ingesting the same slot overwrites: tx_count reflects the latest write.
        #[test]
        fn prop_re_ingest_overwrites(slot in 1u64..=10_000u64, first in 1usize..=5, second in 1usize..=5) {
            let dir = tempfile::TempDir::new().unwrap();
            let store = PersistentStore::open(dir.path()).unwrap();
            store.ingest(&make_block_arb(slot, first)).unwrap();
            store.ingest(&make_block_arb(slot, second)).unwrap();
            let indexed = store.get_block(slot).unwrap().unwrap();
            prop_assert_eq!(indexed.tx_count, second);
        }

        /// latest_slot on an empty store is 0.
        #[test]
        fn prop_empty_store_latest_slot_zero(_dummy in 0u8..=1) {
            let dir = tempfile::TempDir::new().unwrap();
            let store = PersistentStore::open(dir.path()).unwrap();
            prop_assert_eq!(store.latest_slot().unwrap(), 0);
        }
    }
}
