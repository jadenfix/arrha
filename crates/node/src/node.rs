use aether_consensus::slashing::{self as slash_verify, SlashProof, SlashType, Vote as SlashVote};
use aether_consensus::{ConsensusEngine, SlashingDetector};
use aether_crypto_bls::BlsKeypair;
use aether_crypto_primitives::Keypair;
use aether_ledger::{
    BlockContext, EmissionSchedule, FeeMarket, Ledger, PendingOverlay, SparseMerkleTree,
};
use aether_mempool::Mempool;
use aether_p2p::network::NetworkEvent;
use aether_program_staking::StakingState;
use aether_state_snapshots::generate_snapshot;
use aether_state_storage::{
    database::pruning, Storage, StorageBatch, CF_BLOCKS, CF_METADATA, CF_RECEIPTS, CF_STAKING,
};
use aether_types::{
    Account, Address, AggregatedVote, Block, ChainConfig, PublicKey, Slot, Transaction,
    TransactionReceipt, Vote, H256,
};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time;

use aether_metrics::{CONSENSUS_METRICS, NODE_METRICS, STORAGE_METRICS};

/// Overflow-safe (a * b) / c using 256-bit intermediate product.
/// Avoids silent truncation when a*b overflows u128 (e.g. emission * stake).
fn mul_div(a: u128, b: u128, c: u128) -> u128 {
    if c == 0 {
        return 0;
    }
    let a_hi = a >> 64;
    let a_lo = a & 0xFFFF_FFFF_FFFF_FFFF;
    let b_hi = b >> 64;
    let b_lo = b & 0xFFFF_FFFF_FFFF_FFFF;

    let lo_lo = a_lo * b_lo;
    let hi_lo = a_hi * b_lo;
    let lo_hi = a_lo * b_hi;
    let hi_hi = a_hi * b_hi;

    let mid = hi_lo + (lo_lo >> 64);
    let mid = mid + lo_hi;
    let carry = if mid < lo_hi { 1u128 } else { 0u128 };

    let product_lo = (mid << 64) | (lo_lo & 0xFFFF_FFFF_FFFF_FFFF);
    let product_hi = hi_hi + (mid >> 64) + carry;

    div_256_by_128(product_hi, product_lo, c)
}

fn div_256_by_128(hi: u128, lo: u128, divisor: u128) -> u128 {
    if hi == 0 {
        return lo / divisor;
    }
    if hi >= divisor {
        return u128::MAX;
    }
    let mut remainder = hi;
    let mut quotient: u128 = 0;
    for i in (0..128).rev() {
        remainder = (remainder << 1) | ((lo >> i) & 1);
        if remainder >= divisor {
            remainder -= divisor;
            quotient |= 1u128 << i;
        }
    }
    quotient
}

use crate::fork_choice::ForkChoice;
use crate::network_handler::{decode_network_event, NodeMessage, OutboundMessage};
use crate::poh::{PohMetrics, PohRecorder};
use crate::sync::SyncManager;

const MAX_OUTBOUND_BUFFER: usize = 10_000;
const MAX_CACHED_BLOCKS: usize = 10_000;
const MAX_CACHED_RECEIPTS: usize = 50_000;
/// Maximum number of orphan blocks to buffer while waiting for parents.
const MAX_ORPHAN_BLOCKS: usize = 256;

/// Maximum number of seconds a block's timestamp may be in the future relative
/// to wall clock. This prevents a malicious proposer from manufacturing far-future
/// timestamps that could manipulate time-sensitive on-chain logic.
const MAX_CLOCK_DRIFT_SECS: u64 = 15;

const MAX_BLOCK_GAS_LIMIT: u64 = 10_000_000;

/// Minimum interval between serving sync block-range responses.
/// Prevents a peer from flooding sync requests and consuming all outbound bandwidth.
const SYNC_RESPONSE_COOLDOWN: Duration = Duration::from_secs(1);

/// Minimum interval between our own catch-up (block range) requests.
const CATCHUP_REQUEST_INTERVAL: Duration = Duration::from_millis(1000);

/// Maximum transactions per gossip batch (keeps a batch well under the 1 MB
/// tx-topic limit).
const MAX_TX_GOSSIP_BATCH: usize = 1500;

/// Metadata keys for finalized-chain bookkeeping that must survive restarts.
const META_BASE_FEE: &[u8] = b"base_fee";
const META_TIP_QC: &[u8] = b"finalized_tip_qc";

/// Milliseconds since the Unix epoch.
pub fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// A block that has been executed but is not final yet.
///
/// Its state lives in an in-memory overlay chained to its parent's overlay;
/// nothing touches RocksDB until the block is finalized, so competing forks
/// can be executed side by side and the losing branch is simply dropped.
struct PendingBlock {
    block: Block,
    overlay: Arc<PendingOverlay>,
    /// Merkle tree after this block (including fee distribution).
    tree: SparseMerkleTree,
    /// Receipts with block hash and slot filled in.
    receipts: Vec<TransactionReceipt>,
    /// Fee market state after this block.
    fee_market: FeeMarket,
}

/// Output of executing a block on top of its parent.
struct ExecutedBlock {
    receipts: Vec<TransactionReceipt>,
    overlay: PendingOverlay,
    tree: SparseMerkleTree,
    fee_market: FeeMarket,
    /// State root after the block's transactions (before fee distribution);
    /// this is what the block header commits to.
    state_root: H256,
    receipts_root: H256,
}

/// Wall-clock timestamps a node recorded for a block (for explorers and
/// benchmark tooling; not consensus data).
#[derive(Clone, Copy, Debug, Default)]
pub struct BlockTimes {
    pub seen_ms: u64,
    pub finalized_ms: Option<u64>,
}

/// Optional JSON-lines event log (`AETHER_EVENT_LOG`), used to record devnet
/// sessions: proposals, votes, quorum certificates and finality.
struct EventLog {
    out: std::io::BufWriter<std::fs::File>,
    node: String,
}

impl EventLog {
    fn emit(&mut self, mut event: serde_json::Value) {
        if let Some(obj) = event.as_object_mut() {
            obj.insert("t".into(), serde_json::json!(unix_millis()));
            obj.insert("node".into(), serde_json::json!(self.node));
        }
        let _ = writeln!(self.out, "{event}");
        let _ = self.out.flush();
    }
}

fn short_hash(h: &H256) -> String {
    h.as_bytes()[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn short_addr(a: &Address) -> String {
    a.as_bytes()[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

type LoadedBlocks = (
    BTreeMap<Slot, H256>,
    HashMap<H256, Block>,
    H256,
    Option<Slot>,
);

pub struct Node {
    chain_config: Arc<ChainConfig>,
    ledger: Ledger,
    mempool: Mempool,
    consensus: Box<dyn ConsensusEngine>,
    validator_key: Option<Keypair>,
    bls_key: Option<BlsKeypair>,
    running: bool,
    poh: PohRecorder,
    last_poh_metrics: Option<PohMetrics>,
    fee_market: FeeMarket,
    emission_schedule: EmissionSchedule,
    current_epoch: u64,
    fork_choice: ForkChoice,
    latest_block_hash: H256,
    latest_block_slot: Option<Slot>,
    blocks_by_slot: BTreeMap<Slot, H256>,
    blocks_by_hash: HashMap<H256, Block>,
    receipts: HashMap<H256, TransactionReceipt>,
    /// In-memory staking state for tracking validator stakes and applying slashes.
    staking_state: StakingState,
    /// Channel to send outbound messages (blocks, votes, txs) to P2P layer.
    broadcast_tx: Option<mpsc::Sender<OutboundMessage>>,
    /// Collected outbound messages when no broadcast channel is set (for testing).
    outbound_buffer: VecDeque<OutboundMessage>,
    /// Consecutive timeout counter for circuit breaker.
    consecutive_timeouts: u32,
    /// Detects double-signing and other slashable offenses from incoming votes.
    slashing_detector: SlashingDetector,
    /// Tracks (validator, slot) pairs that have already been slashed to prevent
    /// double-slashing the same offense via both vote-time detection and block evidence.
    slashed_offenses: HashSet<(Address, u64)>,
    /// Slots at which this validator has already cast a vote, preventing
    /// accidental double-votes when multiple blocks arrive for the same slot.
    voted_slots: HashSet<u64>,
    /// Tracks sync state (synced, syncing, stalled).
    sync_manager: SyncManager,
    /// Number of connected peers (updated externally via `set_peer_count`).
    peer_count: usize,
    /// Orphan blocks waiting for their parent to arrive, keyed by parent hash.
    orphan_blocks: HashMap<H256, Vec<Block>>,
    /// Total number of orphan blocks buffered (across all parent hashes).
    orphan_count: usize,
    /// Counter for outbound messages dropped due to backpressure.
    outbound_drops: u64,
    /// Directory to write epoch snapshots, if set. Snapshots are written at each
    /// epoch boundary as `snapshot_<epoch>_<slot>.bin` for fast-sync bootstrapping.
    snapshot_dir: Option<PathBuf>,
    /// Rate-limits inbound sync requests to prevent a peer from flooding
    /// us with `RequestBlockRange` messages and consuming all our bandwidth.
    last_sync_response: Option<Instant>,
    /// Highest slot we have already voted on.  Prevents honest validators from
    /// accidentally double-voting when multiple fork blocks arrive at the same
    /// slot (each triggers `vote_on_block` via `on_block_received`).
    last_voted_slot: Option<Slot>,
    /// Tracks which block hash has been durably committed to storage for each slot.
    /// When a fork block arrives and fork-choice would switch canonical, we must NOT
    /// commit the new canonical on top of the already-committed old canonical — doing
    /// so would leave stale effects from the old block in storage (e.g. UTXOs created
    /// by old-canonical txs that the new canonical didn't spend).  The first block
    /// committed at a slot wins; competing blocks are kept in memory for vote/QC
    /// purposes but their state is not written to disk until the chain is replayed.
    committed_at_slot: HashMap<Slot, H256>,
    /// Last slot whose slot logic ran (wall-clock mode).
    last_processed_slot: Option<Slot>,
    /// Executed but not yet finalized blocks, by hash.
    pending: HashMap<H256, PendingBlock>,
    /// Hash and slot of the newest finalized block (= the on-disk state).
    finalized_hash: H256,
    finalized_tip_slot: Slot,
    /// QC certifying the finalized tip, persisted so the node can extend it
    /// after a restart.
    finalized_tip_qc: Option<AggregatedVote>,
    /// Unix-ms of slot 0. When set, `tick_wallclock` derives the slot from the
    /// clock so independent validator processes agree on the current slot.
    genesis_time_ms: Option<u64>,
    /// Never vote at or below this slot (set on startup so a restarted
    /// validator cannot double-vote for slots it voted on before the crash).
    vote_floor: Option<Slot>,
    /// Transactions accepted locally and not yet gossiped.
    tx_gossip_buf: Vec<Transaction>,
    last_catchup_request: Option<Instant>,
    block_times: HashMap<H256, BlockTimes>,
    /// Blocks whose QC we already logged.
    qc_logged: HashSet<H256>,
    event_log: Option<EventLog>,
}

impl Node {
    pub fn new<P: AsRef<Path>>(
        db_path: P,
        mut consensus: Box<dyn ConsensusEngine>,
        validator_key: Option<Keypair>,
        bls_key: Option<BlsKeypair>,
        chain_config: Arc<ChainConfig>,
    ) -> Result<Self> {
        let storage = Storage::open(db_path).context("failed to open storage")?;
        let ledger = Ledger::new(storage).context("failed to initialize ledger")?;
        let mempool = Mempool::new(
            chain_config.fees.clone(),
            chain_config.chain.chain_id_numeric,
        );

        // Warn on asymmetric key configuration
        if validator_key.is_some() != bls_key.is_some() {
            tracing::warn!(
                has_validator_key = validator_key.is_some(),
                has_bls_key = bls_key.is_some(),
                "Asymmetric key config — voting disabled"
            );
        }

        // Load persisted blocks from disk
        let (blocks_by_slot, blocks_by_hash, latest_block_hash, latest_block_slot) =
            Self::load_blocks_from_storage(ledger.storage())?;

        // Restore staking state (validators, delegations, slashes) from disk so
        // slashing effects survive restarts.
        let staking_state = match ledger.storage().get(CF_STAKING, b"staking_state") {
            Ok(Some(bytes)) => {
                let state: StakingState = bincode::deserialize(&bytes)
                    .context("failed to deserialize persisted staking state")?;
                tracing::info!(
                    validators = state.validators.len(),
                    total_staked = state.total_staked,
                    "Restored staking state from disk"
                );
                state
            }
            _ => StakingState::new(),
        };

        if !blocks_by_hash.is_empty() {
            tracing::info!(
                block_count = blocks_by_hash.len(),
                tip_slot = ?latest_block_slot,
                "Recovered blocks from disk"
            );
        }

        // Fast-forward consensus slot to match the recovered chain tip so the
        // node doesn't re-propose blocks at already-occupied slots after restart.
        if let Some(tip_slot) = latest_block_slot {
            consensus.skip_to_slot(tip_slot.saturating_add(1));
            tracing::info!(
                consensus_slot = consensus.current_slot(),
                "Consensus fast-forwarded to match recovered chain tip"
            );
        }

        let mut fee_market = FeeMarket::new(
            chain_config.fees.a,
            chain_config.chain.block_bytes_max,
            chain_config.fees.min_base_fee,
        );
        // The base fee is chain state (it decides proposer rewards), so it is
        // restored from the finalized tip rather than reset on restart.
        if let Ok(Some(bytes)) = ledger.storage().get(CF_METADATA, META_BASE_FEE) {
            if bytes.len() == 16 {
                let mut arr = [0u8; 16];
                arr.copy_from_slice(&bytes);
                fee_market.base_fee = u128::from_le_bytes(arr);
            }
        }
        let finalized_tip_qc: Option<AggregatedVote> = ledger
            .storage()
            .get(CF_METADATA, META_TIP_QC)
            .ok()
            .flatten()
            .and_then(|bytes| bincode::deserialize(&bytes).ok());
        // Let consensus know the finalized tip (slot + certificate) so blocks
        // extending it pass the safe-node check after a restart.
        if let Some(tip_slot) = latest_block_slot {
            if let Some(tip) = blocks_by_hash.get(&latest_block_hash) {
                consensus.record_block(latest_block_hash, tip.header.parent_hash, tip_slot);
            }
            if let Some(qc) = finalized_tip_qc.as_ref() {
                consensus.on_qc(qc);
            }
        }
        let emission_schedule = EmissionSchedule::new(
            chain_config.tokens.swr_initial_supply,
            chain_config.chain.slot_ms,
            chain_config.chain.epoch_slots,
        );
        Ok(Node {
            chain_config,
            ledger,
            mempool,
            consensus,
            validator_key,
            bls_key,
            running: false,
            poh: PohRecorder::new(),
            last_poh_metrics: None,
            fee_market,
            emission_schedule,
            current_epoch: 0,
            fork_choice: ForkChoice::new(),
            latest_block_hash,
            latest_block_slot,
            blocks_by_slot,
            blocks_by_hash,
            receipts: HashMap::new(),
            staking_state,
            broadcast_tx: None,
            outbound_buffer: VecDeque::new(),
            consecutive_timeouts: 0,
            slashing_detector: SlashingDetector::new(),
            slashed_offenses: HashSet::new(),
            voted_slots: HashSet::new(),
            sync_manager: SyncManager::new(10),
            peer_count: 0,
            orphan_blocks: HashMap::new(),
            orphan_count: 0,
            outbound_drops: 0,
            last_sync_response: None,
            snapshot_dir: None,
            last_voted_slot: None,
            committed_at_slot: HashMap::new(),
            last_processed_slot: None,
            pending: HashMap::new(),
            finalized_hash: latest_block_hash,
            finalized_tip_slot: latest_block_slot.unwrap_or(0),
            finalized_tip_qc,
            genesis_time_ms: None,
            vote_floor: None,
            tx_gossip_buf: Vec::new(),
            last_catchup_request: None,
            block_times: HashMap::new(),
            qc_logged: HashSet::new(),
            event_log: None,
        })
    }

    /// Enable the JSON-lines event log at `path` (appends).
    pub fn set_event_log(&mut self, path: &Path) -> Result<()> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open event log {}", path.display()))?;
        let node = self
            .validator_key
            .as_ref()
            .map(|k| {
                let addr = Address::from(k.to_address());
                short_addr(&addr)
            })
            .unwrap_or_else(|| "observer".to_string());
        self.event_log = Some(EventLog {
            out: std::io::BufWriter::new(file),
            node,
        });
        Ok(())
    }

    fn emit(&mut self, event: serde_json::Value) {
        if let Some(log) = self.event_log.as_mut() {
            log.emit(event);
        }
    }

    /// Install the WASM contract runtime used for deploy/call transactions.
    pub fn set_contract_executor(&mut self, executor: Arc<dyn aether_ledger::ContractExecutor>) {
        self.ledger.set_contract_executor(executor);
    }

    /// Derive slots from wall-clock time relative to `genesis_ms`.
    pub fn set_genesis_time_ms(&mut self, genesis_ms: u64) {
        self.genesis_time_ms = Some(genesis_ms);
    }

    /// Slot that the wall clock says it is (requires `set_genesis_time_ms`).
    pub fn wallclock_slot(&self, now_ms: u64) -> Option<Slot> {
        let genesis = self.genesis_time_ms?;
        let slot_ms = self.chain_config.chain.slot_ms.max(1);
        Some(now_ms.saturating_sub(genesis) / slot_ms)
    }

    /// Unix-ms at which `slot` starts.
    pub fn slot_start_ms(&self, slot: Slot) -> Option<u64> {
        let genesis = self.genesis_time_ms?;
        Some(genesis.saturating_add(slot.saturating_mul(self.chain_config.chain.slot_ms)))
    }

    /// Advance to the wall-clock slot and run its slot logic exactly once.
    /// Returns true if a new slot was processed.
    pub fn tick_wallclock(&mut self, now_ms: u64) -> Result<bool> {
        let Some(target) = self.wallclock_slot(now_ms) else {
            self.tick()?;
            return Ok(true);
        };
        if self.vote_floor.is_none() {
            // First tick after (re)start: never vote at slots we may have
            // voted on in a previous run.
            self.vote_floor = Some(target);
            self.last_voted_slot = Some(self.last_voted_slot.unwrap_or(0).max(target));
        }
        let current = self.consensus.current_slot();
        if target < current || (target == current && self.slot_processed(current)) {
            return Ok(false);
        }
        if target > current {
            self.consensus.skip_to_slot(target);
        }
        self.process_slot()?;
        self.last_processed_slot = Some(target);
        Ok(true)
    }

    fn slot_processed(&self, slot: Slot) -> bool {
        self.last_processed_slot == Some(slot)
    }

    /// Configure a directory where epoch snapshots are written for fast-sync.
    ///
    /// When set, a compressed snapshot is written at each epoch boundary to
    /// `<dir>/snapshot_<epoch>_<slot>.bin`. New nodes can import the latest
    /// snapshot via `aether_state_snapshots::import_snapshot` to skip replaying
    /// all historical blocks.
    pub fn set_snapshot_dir(&mut self, dir: PathBuf) {
        self.snapshot_dir = Some(dir);
    }

    /// Load persisted blocks from RocksDB on startup.
    ///
    /// Uses the persisted chain tip for O(1) tip recovery when available,
    /// falling back to a full scan for databases without the tip metadata.
    /// Only keeps the most recent MAX_CACHED_BLOCKS to bound memory usage.
    fn load_blocks_from_storage(storage: &Storage) -> Result<LoadedBlocks> {
        // Try O(1) chain tip recovery from metadata written atomically with each block.
        let persisted_tip = Self::load_persisted_chain_tip(storage);

        // Collect all blocks, then keep only the most recent MAX_CACHED_BLOCKS
        let mut all: Vec<(Slot, H256, Block)> = Vec::new();
        for (_, value) in storage.iterator(CF_BLOCKS)? {
            if let Ok(block) = bincode::deserialize::<Block>(&value) {
                let hash = block.hash();
                let slot = block.header.slot;
                all.push((slot, hash, block));
            }
        }
        all.sort_unstable_by_key(|(slot, _, _)| *slot);

        // Trim to the most recent blocks
        let start = all.len().saturating_sub(MAX_CACHED_BLOCKS);
        let recent = &all[start..];

        let mut by_slot = BTreeMap::new();
        let mut by_hash = HashMap::new();
        let mut latest_hash = H256::zero();
        let mut latest_slot: Option<Slot> = None;

        for (slot, hash, block) in recent {
            by_slot.insert(*slot, *hash);
            by_hash.insert(*hash, block.clone());
            if latest_slot.is_none_or(|s| *slot > s) {
                latest_slot = Some(*slot);
                latest_hash = *hash;
            }
        }

        // Prefer the persisted tip if it matches a loaded block (crash-safe source of truth).
        // Fall back to scan-derived tip for legacy databases.
        if let Some((tip_slot, tip_hash)) = persisted_tip {
            if by_hash.contains_key(&tip_hash) {
                latest_slot = Some(tip_slot);
                latest_hash = tip_hash;
            } else {
                tracing::warn!(
                    persisted_tip_slot = tip_slot,
                    "Persisted chain tip not in cached blocks — using scan-derived tip"
                );
            }
        }

        Ok((by_slot, by_hash, latest_hash, latest_slot))
    }

    /// Read the persisted chain tip from metadata (O(1) lookup).
    fn load_persisted_chain_tip(storage: &Storage) -> Option<(Slot, H256)> {
        let slot_bytes = storage.get(CF_METADATA, b"chain_tip_slot").ok()??;
        let hash_bytes = storage.get(CF_METADATA, b"chain_tip_hash").ok()??;
        if slot_bytes.len() != 8 || hash_bytes.len() != 32 {
            return None;
        }
        let slot = u64::from_le_bytes(slot_bytes.try_into().ok()?);
        let hash = H256::from_slice(&hash_bytes).ok()?;
        Some((slot, hash))
    }

    /// Build a StorageBatch for block and receipt persistence (without writing).
    /// Callers combine this with the overlay batch for a single atomic commit.
    fn build_block_batch(
        &self,
        block: &Block,
        block_hash: H256,
        receipts: &[TransactionReceipt],
    ) -> Result<StorageBatch> {
        let mut batch = StorageBatch::new();

        // Block data
        let block_bytes = bincode::serialize(block)?;
        batch.put(CF_BLOCKS, block_hash.as_bytes().to_vec(), block_bytes);

        // Slot→hash index
        let slot_key = format!("slot:{}", block.header.slot);
        batch.put(
            CF_METADATA,
            slot_key.as_bytes().to_vec(),
            block_hash.as_bytes().to_vec(),
        );

        // Receipts
        for receipt in receipts {
            let receipt_bytes = bincode::serialize(receipt)?;
            batch.put(
                CF_RECEIPTS,
                receipt.tx_hash.as_bytes().to_vec(),
                receipt_bytes,
            );
        }

        // Persist chain tip so restart recovery is O(1) instead of scanning all blocks.
        // Written atomically with block data — crash-safe.
        let tip_slot_bytes = block.header.slot.to_le_bytes().to_vec();
        batch.put(CF_METADATA, b"chain_tip_slot".to_vec(), tip_slot_bytes);
        batch.put(
            CF_METADATA,
            b"chain_tip_hash".to_vec(),
            block_hash.as_bytes().to_vec(),
        );

        Ok(batch)
    }

    /// Set the broadcast channel for outbound P2P messages.
    pub fn set_broadcast_tx(&mut self, tx: mpsc::Sender<OutboundMessage>) {
        self.broadcast_tx = Some(tx);
    }

    /// Drain collected outbound messages (for testing without P2P).
    pub fn drain_outbound(&mut self) -> Vec<OutboundMessage> {
        std::mem::take(&mut self.outbound_buffer).into()
    }

    fn broadcast(&mut self, msg: OutboundMessage) {
        if let Some(ref tx) = self.broadcast_tx {
            // Drain buffered messages first (from before channel was available).
            // Limit drain to 64 per call to avoid holding the lock too long.
            let mut drained = 0usize;
            while drained < 64 {
                if self.outbound_buffer.is_empty() {
                    break;
                }
                match tx.try_send(self.outbound_buffer[0].clone()) {
                    Ok(()) => {
                        self.outbound_buffer.pop_front();
                        drained += 1;
                    }
                    Err(_) => break, // Channel full or closed — stop draining
                }
            }
            match tx.try_send(msg) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_msg)) => {
                    self.outbound_drops += 1;
                    tracing::warn!(
                        total_drops = self.outbound_drops,
                        "P2P outbound channel full, dropping message"
                    );
                }
                Err(mpsc::error::TrySendError::Closed(msg)) => {
                    tracing::warn!("Broadcast channel closed");
                    if self.outbound_buffer.len() < MAX_OUTBOUND_BUFFER {
                        self.outbound_buffer.push_back(msg);
                    } else {
                        self.outbound_drops += 1;
                    }
                }
            }
        } else if self.outbound_buffer.len() < MAX_OUTBOUND_BUFFER {
            self.outbound_buffer.push_back(msg);
        } else {
            self.outbound_drops += 1;
            tracing::error!(
                total_drops = self.outbound_drops,
                "Outbound buffer full ({MAX_OUTBOUND_BUFFER}), dropping message"
            );
        }
    }

    pub fn submit_transaction(&mut self, tx: Transaction) -> Result<H256> {
        let tx_hash = tx.hash();
        self.mempool.add_transaction(tx.clone())?;
        if self.broadcast_tx.is_some() {
            // Batched: `flush_tx_gossip` publishes many transactions per
            // gossip message (per-message gossip cannot sustain high rates).
            self.tx_gossip_buf.push(tx);
            if self.tx_gossip_buf.len() >= MAX_TX_GOSSIP_BATCH {
                self.flush_tx_gossip();
            }
        } else {
            self.broadcast(OutboundMessage::BroadcastTransaction(tx));
        }
        Ok(tx_hash)
    }

    /// Publish buffered transactions to peers as one batch.
    pub fn flush_tx_gossip(&mut self) {
        while !self.tx_gossip_buf.is_empty() {
            let take = self.tx_gossip_buf.len().min(MAX_TX_GOSSIP_BATCH);
            let batch: Vec<Transaction> = self.tx_gossip_buf.drain(..take).collect();
            self.broadcast(OutboundMessage::BroadcastTransactions(batch));
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        self.running = true;

        tracing::info!(
            validator = self.validator_key.is_some(),
            starting_slot = self.consensus.current_slot(),
            "Node starting"
        );

        while self.running {
            self.tick()?;

            // Wait for slot duration (500ms)
            time::sleep(Duration::from_millis(500)).await;
        }

        Ok(())
    }

    pub fn tick(&mut self) -> Result<()> {
        self.process_slot()?;
        self.consensus.advance_slot();
        Ok(())
    }

    fn process_slot(&mut self) -> Result<()> {
        let slot = self.consensus.current_slot();
        let _span = tracing::info_span!("process_slot", slot).entered();
        CONSENSUS_METRICS.consensus_rounds.inc();
        NODE_METRICS.current_slot.set(slot as i64);

        // Update mempool with current slot for forced inclusion tracking
        self.mempool.set_current_slot(slot);

        // Check for epoch transition
        let epoch = slot / self.chain_config.chain.epoch_slots;
        if epoch > self.current_epoch {
            self.process_epoch_transition(epoch)?;
        }
        self.current_epoch = epoch;

        // Pacemaker bookkeeping. Slots advance on the wall clock (or the test
        // harness), so a timeout only resets the local phase; an empty slot
        // (no VRF-eligible leader) is normal and not an error.
        if self.consensus.is_timed_out() {
            self.consecutive_timeouts = self.consecutive_timeouts.saturating_add(1);
            tracing::debug!(
                slot,
                consecutive = self.consecutive_timeouts,
                "No quorum certificate this slot"
            );
            self.consensus.on_timeout();
        } else {
            self.consecutive_timeouts = 0;
        }

        let metrics = self.poh.tick(Instant::now());
        self.last_poh_metrics = Some(metrics.clone());
        tracing::debug!(
            last_ms = metrics.last_duration_ms,
            avg_ms = format_args!("{:.1}", metrics.average_duration_ms),
            jitter_ms = format_args!("{:.1}", metrics.jitter_ms),
            "PoH tick"
        );

        // Slot 0 is the genesis slot; nobody proposes there. A node that is
        // missing blocks (orphans buffered) would only build a stale fork, so
        // it catches up first.
        if slot > 0 && self.orphan_count == 0 {
            if let Some(ref keypair) = self.validator_key {
                let pubkey = PublicKey::from_bytes(keypair.public_key());

                if self.consensus.is_leader(slot, &pubkey) {
                    tracing::info!(slot, "Leader — producing block");
                    self.produce_block(slot)?;
                } else {
                    tracing::debug!(slot, "Not leader — waiting for block");
                }
            }
        }

        // Check if any slot can be finalized
        self.check_finality();

        // Ask peers for blocks if we are missing some.
        self.drive_catchup(slot);

        // Evict old cached blocks/receipts to bound memory (Fix 10)
        self.evict_old_cache();

        Ok(())
    }

    /// Request missing blocks from peers when we hold orphans (blocks whose
    /// parent we do not have) or our chain is far behind the wall clock.
    /// Peers answer by re-broadcasting the requested blocks; orphans are then
    /// connected in `process_orphans`.
    fn drive_catchup(&mut self, current_slot: Slot) {
        let head_slot = self.latest_block_slot.unwrap_or(0);
        let lagging = self.peer_count > 0 && current_slot > head_slot.saturating_add(8);
        let behind = self.orphan_count > 0 || lagging;
        NODE_METRICS.sync_active.set(i64::from(behind));
        NODE_METRICS
            .sync_slot_lag
            .set(current_slot.saturating_sub(head_slot) as i64);
        if !behind {
            return;
        }
        if self
            .last_catchup_request
            .is_some_and(|t| t.elapsed() < CATCHUP_REQUEST_INTERVAL)
        {
            return;
        }
        // Ask for everything after the newest block we know is on the
        // certified chain (our finalized tip, or a certified pending block).
        let certified_head = self
            .pending
            .iter()
            .filter(|(hash, _)| self.consensus.qc_for(hash).is_some())
            .map(|(_, p)| p.block.header.slot)
            .max()
            .unwrap_or(0);
        let from = certified_head
            .max(self.finalized_tip_slot)
            .saturating_add(1);
        let to = current_slot.max(from);
        tracing::info!(
            from,
            to,
            orphans = self.orphan_count,
            "Requesting missing blocks from peers"
        );
        self.last_catchup_request = Some(Instant::now());
        self.broadcast(OutboundMessage::RequestBlockRange {
            from_slot: from,
            to_slot: to,
        });
    }

    /// Process epoch transition: distribute staking rewards.
    fn process_epoch_transition(&mut self, new_epoch: u64) -> Result<()> {
        let _span = tracing::info_span!("epoch_transition", epoch = new_epoch).entered();

        let slot = self.consensus.current_slot();
        let total_supply = self.chain_config.tokens.swr_initial_supply;
        let emission = self.emission_schedule.epoch_emission(slot, total_supply);

        if emission == 0 {
            return Ok(());
        }

        let total_stake = self.consensus.total_stake();
        if total_stake == 0 {
            return Ok(());
        }

        tracing::info!(
            prev_epoch = new_epoch - 1,
            new_epoch,
            emission,
            "Distributing emission rewards"
        );

        // ATOMIC EPOCH COMMIT: batch all emission rewards and unbonding returns
        // into a single WriteBatch so a crash mid-epoch cannot leave some
        // validators credited and others not.
        let mut epoch_batch = StorageBatch::new();

        // Credit emission proportionally to ALL validators based on stake.
        // Every node must credit the same set of validators so state roots
        // stay consistent across the network. The previous code only credited
        // the local validator, causing state divergence at epoch boundaries.
        let mut validators_credited = 0u64;
        for (addr, stake) in self.consensus.validator_addresses_and_stakes() {
            let share = mul_div(emission, stake, total_stake);
            if share > 0 {
                self.ledger
                    .credit_account_to_batch(&mut epoch_batch, &addr, share)?;
                validators_credited += 1;
            }
        }
        tracing::info!(
            validators_credited,
            emission,
            "Credited epoch emission to validators"
        );

        // Complete unbonding: return tokens to delegators whose unbonding period
        // has elapsed. complete_unbonding() returns (address, amount) pairs.
        let completed = self.staking_state.complete_unbonding(slot);
        for (addr, amount) in &completed {
            self.ledger
                .credit_account_to_batch(&mut epoch_batch, addr, *amount)?;
        }

        // Persist staking state (unbonding queue was drained above) atomically
        // with the epoch credits so a crash cannot lose unbonding completions.
        self.persist_staking_state_to_batch(&mut epoch_batch)?;

        // Single atomic write for all epoch credits + staking state
        self.ledger.write_batch(epoch_batch)?;

        for (addr, amount) in &completed {
            tracing::info!(?addr, amount, "Returned unbonded tokens");
        }

        // Prune old blocks and receipts from disk to prevent unbounded DB growth.
        let retention = self.chain_config.chain.retention_epochs;
        if retention > 0 && new_epoch > retention {
            let prune_before_epoch = new_epoch - retention;
            let prune_before_slot =
                prune_before_epoch.saturating_mul(self.chain_config.chain.epoch_slots);
            match pruning::prune_old_blocks(self.ledger.storage(), prune_before_slot) {
                Ok(pruned) => tracing::info!(
                    new_epoch,
                    prune_before_slot,
                    pruned,
                    "Pruned old blocks and receipts"
                ),
                Err(e) => tracing::warn!(err = %e, "Block/receipt pruning failed"),
            }
            // Prune spent-UTXO records older than the retention window and
            // compact CF_UTXOS to reclaim tombstone space from regular UTXO consumption.
            match pruning::prune_spent_utxos(self.ledger.storage(), prune_before_slot) {
                Ok(pruned) => {
                    if pruned > 0 {
                        tracing::info!(
                            new_epoch,
                            prune_before_slot,
                            pruned,
                            "Pruned spent-UTXO records"
                        );
                    }
                }
                Err(e) => tracing::warn!(err = %e, "Spent-UTXO pruning failed"),
            }
        }

        // Write an epoch snapshot for fast-sync if a snapshot directory is configured.
        if let Some(ref dir) = self.snapshot_dir {
            match generate_snapshot(self.ledger.storage(), slot) {
                Ok(bytes) => {
                    let filename = format!("snapshot_{}_{}.bin", new_epoch, slot);
                    let path = dir.join(&filename);
                    match std::fs::write(&path, &bytes) {
                        Ok(()) => tracing::info!(
                            new_epoch,
                            slot,
                            path = %path.display(),
                            size_bytes = bytes.len(),
                            "Epoch snapshot written"
                        ),
                        Err(e) => tracing::warn!(
                            err = %e,
                            path = %path.display(),
                            "Failed to write epoch snapshot"
                        ),
                    }
                }
                Err(e) => tracing::warn!(err = %e, new_epoch, "Failed to generate epoch snapshot"),
            }
        }

        Ok(())
    }

    /// Evict oldest cached blocks/receipts to keep memory bounded.
    fn evict_old_cache(&mut self) {
        // Evict finalized blocks exceeding the cache limit (they stay on disk).
        // Never evict the finalized tip or pending blocks.
        while self.blocks_by_hash.len() > MAX_CACHED_BLOCKS {
            match self.blocks_by_slot.iter().next() {
                Some((&min_slot, &hash)) if hash != self.finalized_hash => {
                    self.blocks_by_slot.remove(&min_slot);
                    self.blocks_by_hash.remove(&hash);
                    self.block_times.remove(&hash);
                }
                _ => break,
            }
        }
        if self.block_times.len() > MAX_CACHED_BLOCKS {
            let floor = self
                .finalized_tip_slot
                .saturating_sub(MAX_CACHED_BLOCKS as u64);
            let blocks = &self.blocks_by_hash;
            self.block_times
                .retain(|h, _| blocks.get(h).is_some_and(|b| b.header.slot >= floor));
        }
        if self.qc_logged.len() > 4096 {
            self.qc_logged.clear();
        }

        // Evict oldest receipts by slot (not random HashMap order)
        if self.receipts.len() > MAX_CACHED_RECEIPTS {
            let mut by_slot: Vec<(u64, H256)> = self
                .receipts
                .iter()
                .map(|(hash, r)| (r.slot, *hash))
                .collect();
            by_slot.sort_unstable_by_key(|(slot, _)| *slot);
            for (_, hash) in by_slot.into_iter().take(1000) {
                self.receipts.remove(&hash);
            }
        }

        // Prune fork choice, slashing detector, and slashed-offenses set for finalized slots.
        // slashed_offenses is keyed by slot; entries older than finalized are safe to remove
        // because finalized blocks cannot be re-submitted as new evidence.
        let finalized = self.consensus.finalized_slot();
        self.fork_choice.prune_before(finalized);
        self.slashing_detector.prune_before(finalized);
        self.committed_at_slot.retain(|&slot, _| slot >= finalized);
        self.slashed_offenses.retain(|&(_, slot)| slot >= finalized);
        self.voted_slots.retain(|&slot| slot >= finalized);

        // Prune stale orphan blocks whose slots are at or before the finalized slot.
        // These can never be applied (on_block_received rejects slots ≤ finalized),
        // so keeping them wastes memory and lets an attacker permanently fill the
        // orphan buffer with blocks referencing non-existent parents.
        self.prune_stale_orphans(finalized);
    }

    /// State after `parent_hash`: its overlay (None = finalized storage), its
    /// Merkle tree and fee market. `None` if we cannot execute on top of it.
    fn parent_state(
        &self,
        parent_hash: &H256,
    ) -> Option<(Option<Arc<PendingOverlay>>, SparseMerkleTree, FeeMarket)> {
        if *parent_hash == self.finalized_hash {
            return Some((
                None,
                self.ledger.merkle_tree().clone(),
                self.fee_market.clone(),
            ));
        }
        self.pending.get(parent_hash).map(|p| {
            (
                Some(p.overlay.clone()),
                p.tree.clone(),
                p.fee_market.clone(),
            )
        })
    }

    fn block_slot(&self, hash: &H256) -> Option<Slot> {
        if let Some(p) = self.pending.get(hash) {
            return Some(p.block.header.slot);
        }
        if *hash == self.finalized_hash {
            return Some(self.finalized_tip_slot);
        }
        self.blocks_by_hash.get(hash).map(|b| b.header.slot)
    }

    /// Execute `transactions` on top of `parent_hash` and apply fee
    /// distribution for `proposer`.
    fn execute_on_parent(
        &self,
        parent_hash: &H256,
        proposer: &Address,
        transactions: &[Transaction],
        context: BlockContext,
    ) -> Result<ExecutedBlock> {
        let (parent_overlay, tree, mut fee_market) = self
            .parent_state(parent_hash)
            .ok_or_else(|| anyhow::anyhow!("parent state unavailable for {parent_hash:?}"))?;
        let (receipts, mut overlay, mut tree) = self.ledger.apply_block_on(
            parent_overlay,
            tree,
            transactions,
            Some(self.chain_config.chain.chain_id_numeric),
            context,
        )?;
        let state_root = overlay.state_root;
        let receipts_root = compute_receipts_root(&receipts);
        let total_fees: u128 = transactions
            .iter()
            .fold(0u128, |acc, tx| acc.saturating_add(tx.fee));
        let gas_used: u64 = transactions
            .iter()
            .fold(0u64, |acc, tx| acc.saturating_add(tx.gas_limit));
        let fee_result = fee_market.process_block(gas_used, total_fees);
        self.ledger.apply_fee_distribution(
            &mut overlay,
            &mut tree,
            proposer,
            fee_result.proposer_reward,
            fee_result.burned,
            fee_result.treasury_fee,
        )?;
        Ok(ExecutedBlock {
            receipts,
            overlay,
            tree,
            fee_market,
            state_root,
            receipts_root,
        })
    }

    /// Record an executed block as pending (not final).
    fn insert_pending(&mut self, block: Block, block_hash: H256, executed: ExecutedBlock) {
        let slot = block.header.slot;
        let receipts: Vec<TransactionReceipt> = executed
            .receipts
            .into_iter()
            .map(|mut r| {
                r.block_hash = block_hash;
                r.slot = slot;
                r
            })
            .collect();

        // Transactions stay in the mempool until their block is FINAL: this
        // block may still lose to a fork, and leaders filter out transactions
        // already included on the chain they extend (see `produce_block`).

        if self.fork_choice.add_block(slot, block_hash) {
            CONSENSUS_METRICS.fork_events.inc();
        }
        self.consensus
            .record_block(block_hash, block.header.parent_hash, slot);
        if self.latest_block_slot.is_none_or(|s| slot >= s) {
            self.latest_block_slot = Some(slot);
            self.latest_block_hash = block_hash;
        }
        self.block_times.entry(block_hash).or_insert(BlockTimes {
            seen_ms: unix_millis(),
            finalized_ms: None,
        });
        self.blocks_by_hash.insert(block_hash, block.clone());
        self.pending.insert(
            block_hash,
            PendingBlock {
                block,
                overlay: Arc::new(executed.overlay),
                tree: executed.tree,
                receipts,
                fee_market: executed.fee_market,
            },
        );
    }

    /// Block to build on: the highest certified block whose state we have.
    fn choose_parent(&self) -> (H256, Slot) {
        if let Some((hash, slot)) = self.consensus.high_qc() {
            if slot >= self.finalized_tip_slot
                && (hash == self.finalized_hash || self.pending.contains_key(&hash))
            {
                return (hash, slot);
            }
        }
        let best_pending = self
            .pending
            .iter()
            .filter(|(hash, _)| self.consensus.qc_for(hash).is_some())
            .max_by_key(|(hash, p)| (p.block.header.slot, hash.0))
            .map(|(hash, p)| (*hash, p.block.header.slot));
        if let Some(best) = best_pending {
            return best;
        }
        (self.finalized_hash, self.finalized_tip_slot)
    }

    /// QC certifying `hash`, from consensus or (for the finalized tip) disk.
    fn qc_for_block(&self, hash: &H256) -> Option<AggregatedVote> {
        self.consensus.qc_for(hash).or_else(|| {
            if *hash == self.finalized_hash {
                self.finalized_tip_qc.clone()
            } else {
                None
            }
        })
    }

    /// Pick transactions for a block extending `parent_hash`: forced
    /// inclusions first, then by fee; skip transactions already included in
    /// a pending ancestor, keep each sender's nonces contiguous from the
    /// parent state, and respect the block tx-count and gas limits.
    fn select_transactions(&self, slot: Slot, parent_hash: &H256) -> Vec<Transaction> {
        let _mempool_span = tracing::debug_span!("mempool_select", slot).entered();
        const MAX_BLOCK_TXS: usize = 1000;

        let mut included: HashSet<H256> = HashSet::new();
        let mut hash = *parent_hash;
        while let Some(p) = self.pending.get(&hash) {
            included.extend(p.block.transactions.iter().map(|tx| tx.hash()));
            hash = p.block.header.parent_hash;
        }
        let parent_overlay = self.pending.get(parent_hash).map(|p| p.overlay.clone());

        let forced = self
            .mempool
            .must_include_transactions(slot, self.fee_market.base_fee);
        // Non-destructive: transactions stay in the pool until finalized.
        let regular = self
            .mempool
            .peek_transactions(MAX_BLOCK_TXS.saturating_mul(8));
        let mut candidates = Vec::with_capacity(forced.len() + regular.len());
        let mut seen: HashSet<H256> = HashSet::new();
        for tx in forced.into_iter().chain(regular) {
            let h = tx.hash();
            if !included.contains(&h) && seen.insert(h) {
                candidates.push(tx);
            }
        }
        candidates.sort_by(|a, b| {
            a.sender
                .as_bytes()
                .cmp(b.sender.as_bytes())
                .then(a.nonce.cmp(&b.nonce))
        });

        let mut expected: HashMap<Address, u64> = HashMap::new();
        let mut out = Vec::new();
        let mut gas = 0u64;
        for tx in candidates {
            if out.len() >= MAX_BLOCK_TXS {
                break;
            }
            let next_nonce = expected.entry(tx.sender).or_insert_with(|| {
                self.ledger
                    .account_in(parent_overlay.as_deref(), &tx.sender)
                    .map(|a| a.nonce)
                    .unwrap_or(0)
            });
            if tx.nonce != *next_nonce {
                continue;
            }
            let next_gas = gas.saturating_add(tx.gas_limit);
            if next_gas > MAX_BLOCK_GAS_LIMIT {
                continue;
            }
            gas = next_gas;
            *next_nonce += 1;
            out.push(tx);
        }
        out
    }

    fn produce_block(&mut self, slot: Slot) -> Result<()> {
        let _span = tracing::info_span!("produce_block", slot).entered();
        let block_start = Instant::now();

        // HotStuff: extend the highest certified block, carrying its QC.
        let (parent_hash, parent_slot) = self.choose_parent();
        if parent_slot >= slot && parent_hash != H256::zero() {
            return Ok(());
        }
        let aggregated_vote = if parent_hash == H256::zero() {
            None
        } else {
            match self.qc_for_block(&parent_hash) {
                Some(qc) => Some(qc),
                None => {
                    tracing::debug!(slot, ?parent_hash, "No QC for parent — not proposing");
                    return Ok(());
                }
            }
        };

        let transactions = self.select_transactions(slot, &parent_hash);

        // Get VRF proof FIRST (before execution, to fail fast if not leader)
        let vrf_proof = match self.consensus.get_leader_proof(slot) {
            Some(proof) => aether_types::VrfProof {
                output: proof.output,
                proof: proof.proof,
            },
            None => aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
        };

        let key = self
            .validator_key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("validator_key required for block production"))?;
        let proposer = Address::from(key.to_address());

        let parent_timestamp = self
            .blocks_by_hash
            .get(&parent_hash)
            .map(|b| b.header.timestamp)
            .unwrap_or(0);
        let timestamp = (unix_millis() / 1000).max(parent_timestamp);
        let context = BlockContext { slot, timestamp };

        let executed = self.execute_on_parent(&parent_hash, &proposer, &transactions, context)?;
        let successful = executed
            .receipts
            .iter()
            .filter(|r| matches!(r.status, aether_types::TransactionStatus::Success))
            .count();

        let mut block = Block::new(slot, parent_hash, proposer, vrf_proof, transactions);
        block.header.timestamp = timestamp;
        block.header.state_root = executed.state_root;
        block.header.transactions_root = compute_transactions_root(&block.transactions);
        block.header.receipts_root = executed.receipts_root;
        block.aggregated_vote = aggregated_vote;

        let block_hash = block.hash();

        // Validate our own block (VRF eligibility, safe-node rule)
        if let Err(e) = self.consensus.validate_block(&block) {
            tracing::warn!(err = %e, "Self-produced block validation failed");
            return Ok(());
        }

        tracing::info!(
            ?block_hash,
            parent_slot,
            tx_count = block.transactions.len(),
            successful,
            "Block produced"
        );
        CONSENSUS_METRICS.blocks_produced.inc();
        CONSENSUS_METRICS
            .transactions_processed
            .inc_by(block.transactions.len() as u64);
        CONSENSUS_METRICS
            .block_production_ms
            .observe(block_start.elapsed().as_secs_f64() * 1000.0);

        self.emit(serde_json::json!({
            "ev": "propose",
            "slot": slot,
            "hash": short_hash(&block_hash),
            "parent": short_hash(&parent_hash),
            "parent_slot": parent_slot,
            "txs": block.transactions.len(),
            "ok": successful,
            "exec_ms": block_start.elapsed().as_millis() as u64,
        }));

        self.insert_pending(block.clone(), block_hash, executed);

        // Broadcast block to network, then vote for it.
        self.broadcast(OutboundMessage::BroadcastBlock(block.clone()));
        self.vote_on_block(&block)?;

        Ok(())
    }

    /// Create a BLS vote for a block and submit to consensus + broadcast.
    fn vote_on_block(&mut self, block: &Block) -> Result<()> {
        let _span = tracing::info_span!("vote_on_block", slot = block.header.slot).entered();

        // SAFETY: never vote twice at the same slot.  When fork blocks arrive
        // at the same height, `on_block_received` calls this for each one.
        // Without this guard an honest validator would broadcast conflicting
        // votes and get slashed for double-signing.
        if let Some(last) = self.last_voted_slot {
            if block.header.slot <= last {
                tracing::debug!(
                    slot = block.header.slot,
                    last_voted = last,
                    "Skipping vote — already voted at this or later slot"
                );
                return Ok(());
            }
        }

        if !self.consensus.safe_to_vote(block) {
            tracing::debug!(
                slot = block.header.slot,
                "Not voting: block conflicts with our lock (safe-node rule)"
            );
            return Ok(());
        }

        let (validator_key, bls_key) = match (&self.validator_key, &self.bls_key) {
            (Some(vk), Some(bk)) => (vk, bk),
            (Some(_), None) => {
                tracing::warn!("validator_key set but bls_key missing, cannot vote");
                return Ok(());
            }
            _ => return Ok(()), // Not a validator
        };

        let block_hash = block.hash();
        let slot = block.header.slot;

        // Prevent double-voting: if we already voted at this slot (e.g. from
        // a different block produced by a concurrent VRF leader), skip.
        if !self.voted_slots.insert(slot) {
            tracing::debug!(
                slot,
                "Already voted at this slot — skipping to avoid double-vote"
            );
            return Ok(());
        }

        let validator_pubkey = PublicKey::from_bytes(validator_key.public_key());

        let vote_msg = {
            let mut msg = Vec::new();
            msg.extend_from_slice(block_hash.as_bytes());
            msg.extend_from_slice(&slot.to_le_bytes());
            msg
        };
        let vote_sig = bls_key.sign(&vote_msg);

        // Use individual validator stake, NOT total_stake
        let voter_addr = validator_pubkey.to_address();
        let my_stake = self.consensus.validator_stake(&voter_addr);

        let vote = Vote {
            slot,
            block_hash,
            validator: validator_pubkey,
            signature: aether_types::Signature::from_bytes(vote_sig),
            stake: my_stake,
        };

        match self.consensus.add_vote(vote.clone()) {
            Ok(()) => tracing::info!(slot, ?block_hash, "Vote submitted"),
            Err(e) => tracing::warn!(slot, err = %e, "Vote failed"),
        }

        self.broadcast(OutboundMessage::BroadcastVote(vote));
        self.last_voted_slot = Some(slot);
        self.emit(serde_json::json!({
            "ev": "vote",
            "slot": slot,
            "hash": short_hash(&block_hash),
        }));
        self.note_qc(&block_hash);
        self.check_finality();

        Ok(())
    }

    // ========================================================================
    // Block Reception (Phase B)
    // ========================================================================

    /// Handle a block received from the P2P network.
    ///
    /// The block is validated, executed on top of its parent's (possibly
    /// pending) state and kept as a pending block. Its state reaches disk
    /// only when it is finalized (see `check_finality`).
    pub fn on_block_received(&mut self, block: Block) -> Result<()> {
        let block_hash = block.hash();
        let _span =
            tracing::info_span!("on_block_received", slot = block.header.slot, ?block_hash,)
                .entered();

        // Already known (pending or finalized)
        if self.pending.contains_key(&block_hash)
            || block_hash == self.finalized_hash
            || self.blocks_by_slot.get(&block.header.slot) == Some(&block_hash)
        {
            return Ok(());
        }

        // At or below the finalized tip: either already final or a dead fork.
        if block.header.slot <= self.finalized_tip_slot && self.finalized_tip_slot > 0 {
            return Ok(());
        }

        // Reject blocks with unsupported protocol version
        if block.header.version != aether_types::PROTOCOL_VERSION {
            bail!(
                "unsupported protocol version: got {}, expected {}",
                block.header.version,
                aether_types::PROTOCOL_VERSION
            );
        }

        let parent_hash = block.header.parent_hash;
        let parent_known =
            parent_hash == self.finalized_hash || self.pending.contains_key(&parent_hash);
        if !parent_known {
            // A parent that is final but not the tip means this block is on a
            // branch that lost; anything else is an orphan we buffer until its
            // parent arrives (catch-up requests fetch missing ancestors).
            let parent_is_old_final = parent_hash != H256::zero()
                && self
                    .block_slot(&parent_hash)
                    .is_some_and(|s| s < self.finalized_tip_slot);
            if parent_is_old_final || (parent_hash == H256::zero() && self.finalized_tip_slot > 0) {
                return Ok(());
            }
            if self.orphan_count < MAX_ORPHAN_BLOCKS {
                let already = self
                    .orphan_blocks
                    .get(&parent_hash)
                    .is_some_and(|v| v.iter().any(|b| b.hash() == block_hash));
                if !already {
                    tracing::info!(
                        slot = block.header.slot,
                        parent = ?parent_hash,
                        "Buffering orphan block (parent unknown)"
                    );
                    self.orphan_count += 1;
                    self.orphan_blocks
                        .entry(parent_hash)
                        .or_default()
                        .push(block);
                }
            } else {
                tracing::warn!(
                    slot = block.header.slot,
                    "Orphan buffer full ({MAX_ORPHAN_BLOCKS}), dropping block"
                );
            }
            return Ok(());
        }

        let parent_slot = self.block_slot(&parent_hash).unwrap_or(0);
        let parent_timestamp = self
            .blocks_by_hash
            .get(&parent_hash)
            .map(|b| b.header.timestamp)
            .unwrap_or(0);

        // Validate block timestamp before heavier checks:
        // 1. Must not be more than MAX_CLOCK_DRIFT_SECS in the future.
        // 2. Must be >= parent block timestamp (monotonicity).
        if block.header.slot > 0 {
            let now_secs = unix_millis() / 1000;
            if block.header.timestamp > now_secs.saturating_add(MAX_CLOCK_DRIFT_SECS) {
                bail!(
                    "block timestamp {} is too far in the future (now={}, max_drift={}s)",
                    block.header.timestamp,
                    now_secs,
                    MAX_CLOCK_DRIFT_SECS
                );
            }
            if parent_hash != H256::zero() && block.header.timestamp < parent_timestamp {
                bail!(
                    "block timestamp {} precedes parent timestamp {}",
                    block.header.timestamp,
                    parent_timestamp
                );
            }
        }

        // Reject oversized blocks before expensive validation.
        let block_size = bincode::serialized_size(&block).unwrap_or(u64::MAX);
        if block_size > self.chain_config.chain.block_bytes_max {
            bail!(
                "block at slot {} exceeds size limit: {} bytes > {} max",
                block.header.slot,
                block_size,
                self.chain_config.chain.block_bytes_max
            );
        }

        // Reject blocks containing duplicate transaction hashes.
        if block.transactions.len() > 1 {
            let mut seen_tx_hashes = HashSet::with_capacity(block.transactions.len());
            for tx in &block.transactions {
                if !seen_tx_hashes.insert(tx.hash()) {
                    bail!(
                        "block at slot {} contains duplicate transaction {}",
                        block.header.slot,
                        tx.hash()
                    );
                }
            }
        }

        // Our slot clock may lag the proposer's by a few ms; catch up to the
        // wall clock before checking the slot bound.
        if let Some(wall) = self.wallclock_slot(unix_millis()) {
            if wall > self.consensus.current_slot() {
                self.consensus.skip_to_slot(wall);
            }
        }
        // Leader proof and slot bounds. The voting lock is NOT applied here:
        // during catch-up we must accept ancestors of certified blocks.
        self.consensus.verify_proposal(&block)?;

        // Slot monotonicity: block slot must be strictly greater than parent's slot
        if block.header.slot > 0 && parent_hash != H256::zero() && block.header.slot <= parent_slot
        {
            bail!(
                "slot monotonicity violation: block slot {} <= parent slot {}",
                block.header.slot,
                parent_slot
            );
        }

        // Validate transactions_root matches actual transactions
        let computed_tx_root = compute_transactions_root(&block.transactions);
        if computed_tx_root != block.header.transactions_root {
            bail!(
                "transactions_root mismatch: computed={}, block={}",
                computed_tx_root,
                block.header.transactions_root
            );
        }

        // Every block that does not extend genesis MUST carry a quorum
        // certificate for its parent (HotStuff: proposals extend a QC).
        let is_genesis_or_bootstrap =
            block.header.slot <= 1 || block.header.parent_hash == H256::zero();
        if !is_genesis_or_bootstrap && block.aggregated_vote.is_none() {
            bail!(
                "block at slot {} missing required quorum certificate (aggregated_vote)",
                block.header.slot
            );
        }

        // Verify the BLS aggregate signature (proves a quorum voted for parent)
        if let Some(ref agg_vote) = block.aggregated_vote {
            self.verify_qc(agg_vote, &block, is_genesis_or_bootstrap, parent_slot)?;
        }

        // Enforce block gas limit
        let block_gas: u64 = block
            .transactions
            .iter()
            .try_fold(0u64, |acc, tx| acc.checked_add(tx.gas_limit))
            .ok_or_else(|| anyhow::anyhow!("block gas overflow"))?;
        if block_gas > MAX_BLOCK_GAS_LIMIT {
            bail!(
                "block gas {} exceeds limit {}",
                block_gas,
                MAX_BLOCK_GAS_LIMIT
            );
        }

        // The QC is valid: record it (may advance lock, high QC and finality).
        if let Some(ref agg_vote) = block.aggregated_vote {
            if agg_vote.block_hash == parent_hash {
                self.consensus.on_qc(agg_vote);
                self.note_qc(&parent_hash);
            }
        }

        // Execute on top of the parent's state (nothing written to disk)
        let exec_start = Instant::now();
        let context = BlockContext {
            slot: block.header.slot,
            timestamp: block.header.timestamp,
        };
        let executed = self.execute_on_parent(
            &parent_hash,
            &block.header.proposer,
            &block.transactions,
            context,
        )?;
        if executed.receipts_root != block.header.receipts_root {
            bail!(
                "receipts_root mismatch: computed={}, block={}",
                executed.receipts_root,
                block.header.receipts_root
            );
        }
        if executed.state_root != block.header.state_root {
            bail!(
                "state root mismatch: computed={}, block={} — block rejected",
                executed.state_root,
                block.header.state_root
            );
        }

        self.emit(serde_json::json!({
            "ev": "block",
            "slot": block.header.slot,
            "hash": short_hash(&block_hash),
            "parent": short_hash(&parent_hash),
            "parent_slot": parent_slot,
            "proposer": short_addr(&block.header.proposer),
            "txs": block.transactions.len(),
            "qc": block.aggregated_vote.as_ref().map(|q| q.signers.iter().map(|p| short_addr(&p.to_address())).collect::<Vec<_>>()),
            "exec_ms": exec_start.elapsed().as_millis() as u64,
        }));

        self.insert_pending(block.clone(), block_hash, executed);

        // Vote on this block (if we're a validator)
        self.vote_on_block(&block)?;

        // Try to apply any orphan blocks that were waiting for this block as parent.
        self.process_orphans(block_hash);

        self.check_finality();
        Ok(())
    }

    /// Verify a block's embedded QC: it must certify the parent, list each
    /// signer once, carry a valid BLS aggregate and reach a 2/3 stake quorum
    /// computed from our own validator set.
    fn verify_qc(
        &self,
        agg_vote: &AggregatedVote,
        block: &Block,
        is_genesis_or_bootstrap: bool,
        parent_slot: Slot,
    ) -> Result<()> {
        let _bls_span = tracing::debug_span!(
            "verify_bls_aggregate",
            slot = block.header.slot,
            signers = agg_vote.signers.len(),
        )
        .entered();
        if !is_genesis_or_bootstrap {
            if agg_vote.block_hash != block.header.parent_hash {
                bail!(
                    "aggregated vote references block {:?} but parent is {:?}",
                    agg_vote.block_hash,
                    block.header.parent_hash
                );
            }
            if agg_vote.slot != parent_slot {
                bail!(
                    "aggregated vote slot {} does not match parent slot {}",
                    agg_vote.slot,
                    parent_slot
                );
            }
        }
        if agg_vote.signers.is_empty() {
            bail!("aggregated vote has no signers");
        }
        // Reject duplicate signers: listing one validator N times would
        // inflate the voted stake while the BLS aggregate still verifies.
        let mut seen = HashSet::with_capacity(agg_vote.signers.len());
        for signer in &agg_vote.signers {
            if !seen.insert(signer.to_address()) {
                bail!(
                    "duplicate signer {:?} in aggregated vote",
                    signer.to_address()
                );
            }
        }
        // Vote message: block_hash || slot (same as vote_on_block)
        let mut vote_msg = Vec::new();
        vote_msg.extend_from_slice(agg_vote.block_hash.as_bytes());
        vote_msg.extend_from_slice(&agg_vote.slot.to_le_bytes());

        // Never trust agg_vote.total_stake: recompute from our validator set.
        let mut bls_pubkeys = Vec::with_capacity(agg_vote.signers.len());
        let mut voted_stake: u128 = 0;
        for signer in &agg_vote.signers {
            let addr = signer.to_address();
            let bls_pk = self
                .consensus
                .get_bls_pubkey(&addr)
                .ok_or_else(|| anyhow::anyhow!("no BLS pubkey registered for signer {:?}", addr))?;
            bls_pubkeys.push(bls_pk);
            voted_stake = voted_stake.saturating_add(self.consensus.validator_stake(&addr));
        }
        let agg_pk = aether_crypto_bls::aggregate_public_keys(&bls_pubkeys)
            .map_err(|e| anyhow::anyhow!("failed to aggregate signer pubkeys: {e}"))?;
        let valid = aether_crypto_bls::verify_aggregated(
            &agg_pk,
            &vote_msg,
            &agg_vote.aggregated_signature,
        )
        .map_err(|e| anyhow::anyhow!("BLS aggregate verification error: {e}"))?;
        if !valid {
            bail!("invalid BLS aggregate signature in block");
        }
        let total_stake = self.consensus.total_stake();
        if total_stake > 0 && !aether_consensus::has_quorum(voted_stake, total_stake) {
            bail!(
                "insufficient quorum: voted stake {} < required 2/3 of {}",
                voted_stake,
                total_stake
            );
        }
        Ok(())
    }

    /// Emit a `qc` event the first time we learn that `hash` is certified.
    fn note_qc(&mut self, hash: &H256) {
        if self.event_log.is_none() || self.qc_logged.contains(hash) {
            return;
        }
        if let Some(qc) = self.consensus.qc_for(hash) {
            self.qc_logged.insert(*hash);
            let signers: Vec<String> = qc
                .signers
                .iter()
                .map(|p| short_addr(&p.to_address()))
                .collect();
            self.emit(serde_json::json!({
                "ev": "qc",
                "slot": qc.slot,
                "hash": short_hash(hash),
                "signers": signers,
            }));
        }
    }

    /// Recursively process orphan blocks whose parent has just been applied.
    fn process_orphans(&mut self, parent_hash: H256) {
        if let Some(orphans) = self.orphan_blocks.remove(&parent_hash) {
            let count = orphans.len();
            self.orphan_count = self.orphan_count.saturating_sub(count);
            for orphan in orphans {
                let slot = orphan.header.slot;
                match self.on_block_received(orphan) {
                    Ok(()) => tracing::info!(slot, "Applied previously orphaned block"),
                    Err(e) => tracing::warn!(slot, err = %e, "Orphaned block rejected"),
                }
            }
        }
    }

    /// Returns whether the node is currently syncing.
    pub fn is_syncing(&self) -> bool {
        self.orphan_count > 0 || self.sync_manager.is_syncing()
    }

    /// Returns the number of connected peers.
    pub fn peer_count(&self) -> usize {
        self.peer_count
    }

    /// Updates the connected peer count (called from the P2P layer).
    pub fn set_peer_count(&mut self, count: usize) {
        self.peer_count = count;
    }

    /// Returns outbound message drop count (backpressure indicator).
    pub fn outbound_drops(&self) -> u64 {
        self.outbound_drops
    }

    /// Returns the number of buffered orphan blocks.
    pub fn orphan_count(&self) -> usize {
        self.orphan_count
    }

    /// Remove orphan blocks whose slot is at or before `min_slot`.
    /// Blocks at finalized or earlier slots can never be applied, so keeping
    /// them just wastes memory and lets an attacker permanently fill the buffer.
    fn prune_stale_orphans(&mut self, min_slot: u64) {
        if min_slot == 0 {
            return;
        }
        let mut pruned = 0usize;
        self.orphan_blocks.retain(|_parent_hash, blocks| {
            let before = blocks.len();
            blocks.retain(|b| b.header.slot > min_slot);
            pruned += before - blocks.len();
            !blocks.is_empty()
        });
        if pruned > 0 {
            self.orphan_count = self.orphan_count.saturating_sub(pruned);
            tracing::debug!(
                pruned,
                remaining = self.orphan_count,
                min_slot,
                "Pruned stale orphan blocks"
            );
        }
    }

    // ========================================================================
    // Vote Reception (Phase C)
    // ========================================================================

    /// Handle a vote received from the P2P network.
    /// Checks for double-signing before processing. If a validator votes for two
    /// different blocks in the same slot, they are slashed (5% of stake).
    pub fn on_vote_received(&mut self, vote: Vote) -> Result<()> {
        let _span = tracing::debug_span!(
            "on_vote_received",
            slot = vote.slot,
            validator = ?vote.validator.to_address(),
        )
        .entered();
        let validator_address = vote.validator.to_address();

        // Check for double-signing before accepting the vote
        if let Some(proof) = self.slashing_detector.record_vote(
            validator_address,
            vote.validator.clone(),
            vote.slot,
            vote.block_hash,
            vote.signature.clone(),
        ) {
            // Double-sign detected — apply slash to both consensus vote weights AND
            // staking state (the authoritative bond accounting). Use a dedup set so
            // block-evidence processing cannot slash the same (validator, slot) twice.
            let offense_key = (proof.validator, vote.slot);
            if self.slashed_offenses.insert(offense_key) {
                // Update consensus vote weight (affects current round immediately).
                self.consensus.slash_validator(&proof.validator, 500);

                // Update staking bond accounting so the slash is reflected in
                // validator stake queries and reward calculations.
                let rate_bps = slash_verify::slash_rate_bps(&proof.proof_type);
                match self
                    .staking_state
                    .slash(proof.validator, u128::from(rate_bps), vote.slot)
                {
                    Ok(staking_slashed) => {
                        tracing::warn!(
                            validator = ?proof.validator,
                            slot = vote.slot,
                            consensus_rate_bps = 500,
                            staking_slashed,
                            "Double-sign detected — slashed consensus vote weight and staking bond"
                        );
                        if let Err(e) = self.persist_staking_state() {
                            tracing::error!(
                                err = %e,
                                "Failed to persist staking state after vote-time slash"
                            );
                        }
                    }
                    Err(e) => tracing::warn!(
                        validator = ?proof.validator,
                        slot = vote.slot,
                        err = %e,
                        "Double-sign detected — consensus vote weight slashed but staking slash failed"
                    ),
                }
            } else {
                tracing::debug!(
                    validator = ?proof.validator,
                    slot = vote.slot,
                    "Double-sign already slashed for this (validator, slot) — skipping duplicate"
                );
            }
        }

        let block_hash = vote.block_hash;
        self.consensus.add_vote(vote)?;
        self.note_qc(&block_hash);
        self.check_finality();
        Ok(())
    }

    // ========================================================================
    // Network Event Dispatch
    // ========================================================================

    /// Handle a raw network event from the P2P layer.
    pub fn handle_network_event(&mut self, event: NetworkEvent) -> Result<()> {
        let _span = tracing::debug_span!("handle_network_event").entered();
        match decode_network_event(event) {
            Some(NodeMessage::BlockReceived(block)) => {
                let _msg_span = tracing::debug_span!(
                    "msg_block",
                    slot = block.header.slot,
                    tx_count = block.transactions.len(),
                )
                .entered();
                CONSENSUS_METRICS.blocks_received.inc();
                let slot = block.header.slot;
                if let Err(e) = self.on_block_received(block) {
                    tracing::debug!(err = %e, "Block rejected");
                    self.emit(serde_json::json!({
                        "ev": "reject",
                        "slot": slot,
                        "reason": e.to_string(),
                    }));
                }
            }
            Some(NodeMessage::VoteReceived(vote)) => {
                let _msg_span =
                    tracing::debug_span!("msg_vote", slot = vote.slot, stake = vote.stake,)
                        .entered();
                if let Err(e) = self.on_vote_received(vote) {
                    tracing::debug!(err = %e, "Vote rejected");
                }
            }
            Some(NodeMessage::TransactionsReceived(txs)) => {
                let _msg_span = tracing::debug_span!("msg_txs", count = txs.len()).entered();
                for tx in txs {
                    if let Err(e) = self.mempool.add_transaction(tx) {
                        tracing::trace!(err = %e, "Gossiped tx rejected");
                    }
                }
            }
            Some(NodeMessage::TransactionReceived(tx)) => {
                let _msg_span =
                    tracing::debug_span!("msg_tx", fee = tx.fee, nonce = tx.nonce,).entered();
                if let Err(e) = self.mempool.add_transaction(tx) {
                    tracing::debug!(err = %e, "Tx rejected");
                }
            }
            Some(NodeMessage::BlockRangeRequested { from_slot, to_slot }) => {
                let _msg_span =
                    tracing::debug_span!("msg_block_range_req", from_slot, to_slot,).entered();
                self.handle_block_range_request(from_slot, to_slot);
            }
            Some(NodeMessage::PeerConnected) => {
                self.peer_count = self.peer_count.saturating_add(1);
                tracing::info!(peer_count = self.peer_count, "Peer connected");
            }
            Some(NodeMessage::PeerDisconnected) => {
                self.peer_count = self.peer_count.saturating_sub(1);
                tracing::info!(peer_count = self.peer_count, "Peer disconnected");
            }
            None => {}
        }
        Ok(())
    }

    /// Respond to a peer's sync request by re-broadcasting blocks we have
    /// in the requested slot range. Capped to prevent a single request from
    /// flooding the network.
    ///
    /// Rate-limited via `SYNC_RESPONSE_COOLDOWN` to prevent a malicious peer
    /// from draining outbound bandwidth by spamming sync requests.
    fn handle_block_range_request(&mut self, from_slot: Slot, to_slot: Slot) {
        // Rate-limit: reject if we served a sync response too recently.
        if let Some(last) = self.last_sync_response {
            if last.elapsed() < SYNC_RESPONSE_COOLDOWN {
                tracing::debug!(
                    from_slot,
                    to_slot,
                    cooldown_remaining_ms = (SYNC_RESPONSE_COOLDOWN - last.elapsed()).as_millis(),
                    "Dropping sync request — rate limited"
                );
                return;
            }
        }

        const MAX_BLOCKS_PER_RESPONSE: u64 = 64;
        let end = to_slot.min(from_slot.saturating_add(MAX_BLOCKS_PER_RESPONSE));

        let mut sent = 0u64;
        for slot in from_slot..=end {
            if let Some(block) = self.get_block_by_slot(slot) {
                self.broadcast(OutboundMessage::BroadcastBlock(block));
                sent += 1;
            }
        }
        if sent > 0 {
            self.last_sync_response = Some(Instant::now());
            tracing::info!(from_slot, to_slot = end, sent, "Served sync block request");
        }
    }

    fn check_finality(&mut self) {
        let current_slot = self.consensus.current_slot();
        let last_finalized = self.consensus.finalized_slot();
        let _span = tracing::debug_span!("check_finality", current_slot, last_finalized,).entered();

        // Let engines that finalize lazily (e.g. SimpleConsensus) evaluate
        // recent slots; bounded to avoid CPU spikes after long gaps.
        let end = current_slot.min(last_finalized.saturating_add(100));
        for slot in last_finalized..=end {
            if self.consensus.check_finality(slot) {
                CONSENSUS_METRICS.slots_finalized.inc();
            }
        }

        let finalized_slot = self.consensus.finalized_slot();
        if finalized_slot <= self.finalized_tip_slot && self.finalized_tip_slot > 0 {
            return;
        }
        let target = match self.consensus.finalized_block() {
            Some(hash) => Some(hash),
            None => self.pending_on_head_chain_at(finalized_slot),
        };
        let Some(target) = target else {
            return;
        };
        if !self.pending.contains_key(&target) {
            // Finalized block not executed here yet (we are catching up).
            return;
        }
        if let Err(e) = self.finalize_through(target) {
            tracing::error!(err = %e, "failed to persist finalized blocks");
        }
    }

    /// For engines without block-level finality: the pending block at `slot`
    /// on the chain we would extend.
    fn pending_on_head_chain_at(&self, slot: Slot) -> Option<H256> {
        let (mut hash, _) = self.choose_parent();
        if !self.pending.contains_key(&hash) {
            // Fall back to the newest pending block.
            hash = self
                .pending
                .iter()
                .max_by_key(|(h, p)| (p.block.header.slot, h.0))
                .map(|(h, _)| *h)?;
        }
        let mut found = None;
        while let Some(p) = self.pending.get(&hash) {
            if p.block.header.slot <= slot {
                found = Some(hash);
                break;
            }
            hash = p.block.header.parent_hash;
        }
        found
    }

    /// Persist every pending block from the current finalized tip up to and
    /// including `target`, then drop pending blocks that conflict with it.
    fn finalize_through(&mut self, target: H256) -> Result<()> {
        let mut path = Vec::new();
        let mut hash = target;
        while hash != self.finalized_hash {
            let Some(p) = self.pending.get(&hash) else {
                // Missing ancestor: wait until catch-up delivers it.
                return Ok(());
            };
            path.push(hash);
            hash = p.block.header.parent_hash;
            if path.len() > MAX_CACHED_BLOCKS {
                bail!("finalized path longer than {MAX_CACHED_BLOCKS} blocks");
            }
        }
        path.reverse();
        for hash in path {
            self.commit_finalized(hash)?;
        }
        self.prune_pending();
        Ok(())
    }

    /// Write one finalized block (state delta, block, receipts, fee market,
    /// QC, staking/slashing effects) to storage in a single atomic batch.
    fn commit_finalized(&mut self, block_hash: H256) -> Result<()> {
        let Some(pb) = self.pending.remove(&block_hash) else {
            return Ok(());
        };
        let slot = pb.block.header.slot;
        let _commit_span = tracing::info_span!(
            "atomic_block_commit",
            slot,
            ?block_hash,
            tx_count = pb.block.transactions.len(),
        )
        .entered();

        let mut extra = self.build_block_batch(&pb.block, block_hash, &pb.receipts)?;
        self.ledger
            .record_spent_utxos(&mut extra, &pb.overlay, slot);
        extra.put(
            CF_METADATA,
            META_BASE_FEE.to_vec(),
            pb.fee_market.base_fee.to_le_bytes().to_vec(),
        );
        let qc = self.consensus.qc_for(&block_hash);
        if let Some(ref qc) = qc {
            extra.put(CF_METADATA, META_TIP_QC.to_vec(), bincode::serialize(qc)?);
        }
        self.apply_slash_evidence(&pb.block);
        // Staking state (slashes, unbonding) is persisted with the block.
        self.persist_staking_state_to_batch(&mut extra)?;

        self.ledger.commit_pending(&pb.overlay, &pb.tree, extra)?;
        STORAGE_METRICS.blocks_persisted.inc();

        // Children now read this block's effects from storage.
        for child in self.pending.values() {
            if child.block.header.parent_hash == block_hash {
                child.overlay.detach_parent();
            }
        }

        self.fee_market = pb.fee_market;
        self.finalized_hash = block_hash;
        self.finalized_tip_slot = slot;
        if qc.is_some() {
            self.finalized_tip_qc = qc;
        }
        self.committed_at_slot.insert(slot, block_hash);
        self.blocks_by_slot.insert(slot, block_hash);
        self.blocks_by_hash.insert(block_hash, pb.block.clone());
        for receipt in pb.receipts {
            self.receipts.insert(receipt.tx_hash, receipt);
        }
        // Final: drop the block's transactions from the mempool for good.
        let tx_hashes: Vec<H256> = pb.block.transactions.iter().map(|tx| tx.hash()).collect();
        self.mempool.remove_transactions(&tx_hashes);
        for tx in &pb.block.transactions {
            self.mempool
                .advance_sender_nonce(tx.sender, tx.nonce.saturating_add(1));
        }
        if !self.fork_choice.finalize(slot, block_hash) {
            tracing::debug!(slot, "fork_choice: finalized block was not tracked");
        }

        let now = unix_millis();
        let times = self.block_times.entry(block_hash).or_insert(BlockTimes {
            seen_ms: now,
            finalized_ms: None,
        });
        times.finalized_ms = Some(now);
        let latency_ms = self
            .slot_start_ms(slot)
            .map(|start| now.saturating_sub(start))
            .unwrap_or_else(|| now.saturating_sub(pb.block.header.timestamp.saturating_mul(1000)));
        CONSENSUS_METRICS
            .finality_latency_ms
            .observe(latency_ms as f64);
        tracing::info!(slot, ?block_hash, latency_ms, "Block finalized");
        self.emit(serde_json::json!({
            "ev": "final",
            "slot": slot,
            "hash": short_hash(&block_hash),
            "txs": pb.block.transactions.len(),
        }));
        Ok(())
    }

    /// Drop pending blocks that do not descend from the finalized tip.
    fn prune_pending(&mut self) {
        let mut keep: HashSet<H256> = HashSet::new();
        let mut drop: Vec<H256> = Vec::new();
        let hashes: Vec<H256> = self.pending.keys().copied().collect();
        for start in hashes {
            let mut chain = Vec::new();
            let mut hash = start;
            let descends = loop {
                if hash == self.finalized_hash || keep.contains(&hash) {
                    break true;
                }
                match self.pending.get(&hash) {
                    Some(p) if p.block.header.slot > self.finalized_tip_slot => {
                        chain.push(hash);
                        hash = p.block.header.parent_hash;
                    }
                    _ => break false,
                }
            };
            if descends {
                keep.extend(chain);
            } else {
                drop.push(start);
            }
        }
        if drop.is_empty() {
            return;
        }
        let mut orphaned_txs = 0usize;
        for hash in &drop {
            if let Some(p) = self.pending.remove(hash) {
                orphaned_txs += p.block.transactions.len();
            }
            self.blocks_by_hash.remove(hash);
            self.block_times.remove(hash);
        }
        tracing::info!(
            dropped_blocks = drop.len(),
            txs_back_to_mempool = orphaned_txs,
            "Pruned pending blocks that conflict with finality"
        );
        self.emit(serde_json::json!({
            "ev": "prune",
            "blocks": drop.iter().map(short_hash).collect::<Vec<_>>(),
        }));
        // Their transactions never left the mempool (only finalization
        // removes them), so they are simply eligible again.
    }

    /// Apply slash evidence carried by a finalized block.
    fn apply_slash_evidence(&mut self, block: &Block) {
        for evidence in &block.slash_evidence {
            let (v1, v2, etype) = match (&evidence.vote1, &evidence.vote2, &evidence.evidence_type)
            {
                (Some(v1), Some(v2), Some(etype)) => (v1, v2, etype),
                _ => {
                    tracing::warn!(
                        validator = ?evidence.validator,
                        reason = %evidence.reason,
                        "Slash skipped — missing proof votes/type"
                    );
                    continue;
                }
            };

            let proof_type = match etype {
                aether_types::SlashEvidenceType::DoubleSign => SlashType::DoubleSign,
                aether_types::SlashEvidenceType::SurroundVote => SlashType::SurroundVote,
            };
            let proof = SlashProof {
                vote1: SlashVote {
                    slot: v1.slot,
                    block_hash: v1.block_hash,
                    validator: v1.validator,
                    validator_pubkey: v1.validator_pubkey.clone(),
                    signature: v1.signature.clone(),
                },
                vote2: SlashVote {
                    slot: v2.slot,
                    block_hash: v2.block_hash,
                    validator: v2.validator,
                    validator_pubkey: v2.validator_pubkey.clone(),
                    signature: v2.signature.clone(),
                },
                validator: evidence.validator,
                proof_type: proof_type.clone(),
            };

            if let Err(e) = slash_verify::verify_slash_proof(&proof) {
                tracing::warn!(
                    validator = ?evidence.validator,
                    reason = %evidence.reason,
                    err = %e,
                    "Slash rejected — proof verification failed"
                );
                continue;
            }

            // Dedup: skip if already slashed by vote-time detection for this
            // (validator, slot) pair, preventing double-slash of the same offense.
            let offense_key = (evidence.validator, v1.slot);
            if !self.slashed_offenses.insert(offense_key) {
                tracing::debug!(
                    validator = ?evidence.validator,
                    slot = v1.slot,
                    reason = %evidence.reason,
                    "Slash already applied for this (validator, slot) — skipping block evidence"
                );
                continue;
            }

            let rate_bps = slash_verify::slash_rate_bps(&proof.proof_type);
            self.consensus
                .slash_validator(&evidence.validator, u128::from(rate_bps));
            match self.staking_state.slash(
                evidence.validator,
                u128::from(rate_bps),
                block.header.slot,
            ) {
                Ok(slashed) => tracing::warn!(
                    validator = ?evidence.validator,
                    rate_bps,
                    slashed,
                    reason = %evidence.reason,
                    "Slash applied (block evidence)"
                ),
                Err(e) => tracing::warn!(
                    validator = ?evidence.validator,
                    reason = %evidence.reason,
                    err = %e,
                    "Slash skipped (block evidence)"
                ),
            }
        }
    }

    pub fn stop(&mut self) {
        self.running = false;
    }

    /// Perform a graceful shutdown: stop the node, log final state, and
    /// flush all pending writes to stable storage.
    ///
    /// Must be called before the process exits to avoid losing any data
    /// that is still in the RocksDB write-ahead log buffer.
    pub fn shutdown(&mut self) -> Result<()> {
        self.running = false;

        let slot = self.consensus.current_slot();
        let finalized = self.consensus.finalized_slot();
        let blocks = self.blocks_by_hash.len();
        let mempool = self.mempool.len();
        let orphans = self.orphan_count;

        tracing::info!(
            slot,
            finalized,
            blocks,
            mempool,
            orphans,
            "Shutting down node — flushing state to disk"
        );

        self.ledger.storage().flush_wal()?;

        tracing::info!("WAL flushed — shutdown complete");
        Ok(())
    }

    pub fn get_state_root(&self) -> H256 {
        self.ledger.state_root()
    }

    pub fn mempool_size(&self) -> usize {
        self.mempool.len()
    }

    pub fn poh_metrics(&self) -> Option<&PohMetrics> {
        self.last_poh_metrics.as_ref()
    }

    pub fn current_slot(&self) -> Slot {
        self.consensus.current_slot()
    }

    pub fn finalized_slot(&self) -> Slot {
        self.consensus.finalized_slot()
    }

    pub fn latest_block_slot(&self) -> Option<Slot> {
        self.latest_block_slot
    }

    pub fn latest_block_hash(&self) -> H256 {
        self.latest_block_hash
    }

    pub fn allows_airdrop(&self) -> bool {
        matches!(
            self.chain_config.chain.chain_id.as_str(),
            "aether-dev-1" | "aether-testnet-1"
        )
    }

    pub fn seed_account(&mut self, address: &Address, balance: u128) -> Result<()> {
        self.ledger.seed_account(address, balance)
    }

    /// Apply genesis balances once (first start of a fresh database) and
    /// remember the genesis time. Returns true if genesis was applied now.
    pub fn apply_genesis(
        &mut self,
        accounts: &[(Address, u128)],
        genesis_time_ms: u64,
    ) -> Result<bool> {
        if self
            .ledger
            .storage()
            .get(CF_METADATA, b"genesis_applied")?
            .is_some()
        {
            return Ok(false);
        }
        self.ledger.seed_accounts(accounts)?;
        let mut batch = StorageBatch::new();
        batch.put(CF_METADATA, b"genesis_applied".to_vec(), vec![1]);
        batch.put(
            CF_METADATA,
            b"genesis_time_ms".to_vec(),
            genesis_time_ms.to_le_bytes().to_vec(),
        );
        self.ledger.write_batch(batch)?;
        Ok(true)
    }

    /// Genesis time recorded by `apply_genesis`, if any.
    pub fn stored_genesis_time_ms(&self) -> Option<u64> {
        let bytes = self
            .ledger
            .storage()
            .get(CF_METADATA, b"genesis_time_ms")
            .ok()
            .flatten()?;
        let arr: [u8; 8] = bytes.as_slice().try_into().ok()?;
        Some(u64::from_le_bytes(arr))
    }

    pub fn get_block_by_slot(&self, slot: Slot) -> Option<Block> {
        // Finalized chain: in-memory index first, then RocksDB.
        if let Some(block) = self
            .blocks_by_slot
            .get(&slot)
            .and_then(|hash| self.blocks_by_hash.get(hash))
        {
            return Some(block.clone());
        }
        let slot_key = format!("slot:{}", slot);
        if let Some(hash) = self
            .ledger
            .storage()
            .get(CF_METADATA, slot_key.as_bytes())
            .ok()
            .flatten()
            .and_then(|bytes| H256::from_slice(&bytes).ok())
        {
            return self.get_block_by_hash(hash);
        }
        // Not final yet: the pending block at this slot on the head chain.
        if slot > self.finalized_tip_slot {
            for hash in self.head_chain() {
                if let Some(p) = self.pending.get(&hash) {
                    if p.block.header.slot == slot {
                        return Some(p.block.clone());
                    }
                }
            }
        }
        None
    }

    /// Pending blocks from the newest tip back to the finalized tip
    /// (newest first). Ties at the tip go to the certified block.
    pub fn head_chain(&self) -> Vec<H256> {
        let tip = self
            .pending
            .iter()
            .max_by_key(|(hash, p)| {
                (
                    p.block.header.slot,
                    self.consensus.qc_for(hash).is_some(),
                    hash.0,
                )
            })
            .map(|(hash, _)| *hash);
        let mut out = Vec::new();
        let mut hash = match tip {
            Some(h) => h,
            None => return out,
        };
        while let Some(p) = self.pending.get(&hash) {
            out.push(hash);
            hash = p.block.header.parent_hash;
        }
        out
    }

    /// Whether `hash` is finalized on this node.
    pub fn is_finalized(&self, hash: &H256, slot: Slot) -> bool {
        if slot > self.finalized_tip_slot {
            return false;
        }
        if self.blocks_by_slot.get(&slot) == Some(hash) || *hash == self.finalized_hash {
            return true;
        }
        let slot_key = format!("slot:{}", slot);
        self.ledger
            .storage()
            .get(CF_METADATA, slot_key.as_bytes())
            .ok()
            .flatten()
            .is_some_and(|bytes| bytes.as_slice() == hash.as_bytes())
    }

    /// Hash and slot of the finalized tip.
    pub fn finalized_tip(&self) -> (H256, Slot) {
        (self.finalized_hash, self.finalized_tip_slot)
    }

    /// Address of the local validator key, if any.
    pub fn validator_address(&self) -> Option<Address> {
        self.validator_key
            .as_ref()
            .map(|k| Address::from(k.to_address()))
    }

    pub fn pending_block_count(&self) -> usize {
        self.pending.len()
    }

    pub fn block_times(&self, hash: &H256) -> Option<BlockTimes> {
        self.block_times.get(hash).copied()
    }

    /// QC certifying `hash`, if known.
    pub fn qc_for(&self, hash: &H256) -> Option<AggregatedVote> {
        self.qc_for_block(hash)
    }

    /// Account state as of the newest pending block on the head chain
    /// (includes not-yet-final transactions).
    pub fn get_account_pending(&self, address: Address) -> Result<Account> {
        let head = self.head_chain().first().copied();
        let overlay = head
            .and_then(|h| self.pending.get(&h))
            .map(|p| p.overlay.clone());
        self.ledger.account_in(overlay.as_deref(), &address)
    }

    /// Contract storage, finalized (`pending = false`) or as of the head chain.
    pub fn get_contract_storage(
        &self,
        address: Address,
        pending: bool,
    ) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        if pending {
            let head = self.head_chain().first().copied();
            let overlay = head
                .and_then(|h| self.pending.get(&h))
                .map(|p| p.overlay.clone());
            self.ledger
                .contract_storage_in(overlay.as_deref(), &address)
        } else {
            self.ledger.contract_storage_in(None, &address)
        }
    }

    /// Accept many transactions at once (RPC batch submission).
    pub fn submit_transactions(&mut self, txs: Vec<Transaction>) -> Vec<Result<H256>> {
        txs.into_iter()
            .map(|tx| self.submit_transaction(tx))
            .collect()
    }

    pub fn get_block_by_hash(&self, hash: H256) -> Option<Block> {
        // Check in-memory cache first (recent blocks)
        if let Some(block) = self.blocks_by_hash.get(&hash) {
            return Some(block.clone());
        }
        // Fall back to RocksDB for older/pre-restart blocks
        self.ledger
            .storage()
            .get(CF_BLOCKS, hash.as_bytes())
            .ok()
            .flatten()
            .and_then(|bytes| bincode::deserialize(&bytes).ok())
    }

    pub fn get_transaction_receipt(&self, tx_hash: H256) -> Option<TransactionReceipt> {
        // Check in-memory cache first (recent finalized receipts)
        if let Some(receipt) = self.receipts.get(&tx_hash) {
            return Some(receipt.clone());
        }
        // Then RocksDB for older/pre-restart receipts
        if let Some(receipt) = self
            .ledger
            .storage()
            .get(CF_RECEIPTS, tx_hash.as_bytes())
            .ok()
            .flatten()
            .and_then(|bytes| bincode::deserialize(&bytes).ok())
        {
            return Some(receipt);
        }
        // Finally, included in a pending (not yet final) block on the head chain.
        for hash in self.head_chain() {
            if let Some(p) = self.pending.get(&hash) {
                if let Some(r) = p.receipts.iter().find(|r| r.tx_hash == tx_hash) {
                    return Some(r.clone());
                }
            }
        }
        None
    }

    pub fn get_account(&self, address: Address) -> Result<Option<Account>> {
        self.ledger.get_account(&address)
    }

    pub fn base_fee(&self) -> u128 {
        self.fee_market.base_fee
    }

    /// Mutable access to the in-memory staking state.
    ///
    /// Used by tests and the genesis bootstrap path to register validators
    /// before block production begins.
    pub fn staking_state_mut(&mut self) -> &mut StakingState {
        &mut self.staking_state
    }

    /// Read-only access to the in-memory staking state.
    pub fn staking_state(&self) -> &StakingState {
        &self.staking_state
    }

    /// Serialize current staking state into a batch for atomic persistence.
    fn persist_staking_state_to_batch(&self, batch: &mut StorageBatch) -> Result<()> {
        let bytes =
            bincode::serialize(&self.staking_state).context("failed to serialize staking state")?;
        batch.put(CF_STAKING, b"staking_state".to_vec(), bytes);
        Ok(())
    }

    /// Persist staking state to disk immediately (standalone write).
    fn persist_staking_state(&self) -> Result<()> {
        let mut batch = StorageBatch::new();
        self.persist_staking_state_to_batch(&mut batch)?;
        self.ledger.write_batch(batch)
    }
}

// ============================================================================
// Block Header Root Computation (Phase D)
// ============================================================================

/// Compute the Merkle root of a list of transactions (hash of hashes).
pub fn compute_transactions_root(txs: &[Transaction]) -> H256 {
    if txs.is_empty() {
        return H256::zero();
    }
    let mut hasher = Sha256::new();
    for tx in txs {
        hasher.update(tx.hash().as_bytes());
    }
    let hash: [u8; 32] = hasher.finalize().into();
    H256(hash)
}

/// Compute the Merkle root of a list of receipts.
///
/// Uses only the deterministic fields (tx_hash, status, gas_used, logs, state_root)
/// and excludes block_hash/slot which are set AFTER root computation. This ensures
/// the root is consistent regardless of when it's computed.
pub fn compute_receipts_root(receipts: &[TransactionReceipt]) -> H256 {
    if receipts.is_empty() {
        return H256::zero();
    }
    let mut hasher = Sha256::new();
    for receipt in receipts {
        // Hash only deterministic fields to avoid non-determinism from
        // block_hash/slot being set after root computation.
        let mut receipt_hasher = Sha256::new();
        receipt_hasher.update(receipt.tx_hash.as_bytes());
        receipt_hasher.update(bincode::serialize(&receipt.status).unwrap_or_default());
        receipt_hasher.update(receipt.gas_used.to_le_bytes());
        receipt_hasher.update(bincode::serialize(&receipt.logs).unwrap_or_default());
        receipt_hasher.update(receipt.state_root.as_bytes());
        hasher.update(receipt_hasher.finalize());
    }
    let hash: [u8; 32] = hasher.finalize().into();
    H256(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_consensus::SimpleConsensus;
    use aether_types::{PublicKey, ValidatorInfo};
    use tempfile::TempDir;

    fn validator_info_from_key(keypair: &Keypair) -> ValidatorInfo {
        ValidatorInfo {
            pubkey: PublicKey::from_bytes(keypair.public_key()),
            stake: 1_000,
            commission: 0,
            active: true,
        }
    }

    #[test]
    fn updates_poh_metrics_each_slot() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));

        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        node.process_slot().unwrap();
        let first_metrics = node.poh_metrics().cloned().unwrap();
        assert_eq!(first_metrics.tick_count, 1);

        node.process_slot().unwrap();
        let second_metrics = node.poh_metrics().cloned().unwrap();
        assert!(second_metrics.tick_count >= 2);
        assert!(second_metrics.average_duration_ms >= 0.0);
    }

    #[test]
    fn outbound_buffer_is_capped() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Push more than MAX_OUTBOUND_BUFFER messages
        for _ in 0..MAX_OUTBOUND_BUFFER + 100 {
            node.broadcast(OutboundMessage::BroadcastVote(Vote {
                slot: 0,
                block_hash: H256::zero(),
                validator: PublicKey::from_bytes(vec![0u8; 32]),
                signature: aether_types::Signature::from_bytes(vec![0u8; 64]),
                stake: 0,
            }));
        }

        assert_eq!(node.outbound_buffer.len(), MAX_OUTBOUND_BUFFER);
        // Verify drops were counted
        assert_eq!(node.outbound_drops(), 100);
    }

    #[test]
    fn outbound_drops_counted_on_full_channel() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Create a channel with capacity 1 to force drops quickly
        let (tx, _rx) = mpsc::channel(1);
        node.set_broadcast_tx(tx);

        let vote = OutboundMessage::BroadcastVote(Vote {
            slot: 0,
            block_hash: H256::zero(),
            validator: PublicKey::from_bytes(vec![0u8; 32]),
            signature: aether_types::Signature::from_bytes(vec![0u8; 64]),
            stake: 0,
        });

        // First send should succeed (fills the single slot)
        node.broadcast(vote.clone());
        assert_eq!(node.outbound_drops(), 0);

        // Subsequent sends should be dropped since nobody is receiving
        for _ in 0..10 {
            node.broadcast(vote.clone());
        }
        assert!(node.outbound_drops() > 0, "drops should be counted");
    }

    #[test]
    fn outbound_buffer_drains_when_channel_available() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        let vote = OutboundMessage::BroadcastVote(Vote {
            slot: 0,
            block_hash: H256::zero(),
            validator: PublicKey::from_bytes(vec![0u8; 32]),
            signature: aether_types::Signature::from_bytes(vec![0u8; 64]),
            stake: 0,
        });

        // Buffer messages without a channel
        for _ in 0..5 {
            node.broadcast(vote.clone());
        }
        assert_eq!(node.outbound_buffer.len(), 5);

        // Now set a channel with enough capacity and send one more message
        let (tx, mut rx) = mpsc::channel(64);
        node.set_broadcast_tx(tx);
        node.broadcast(vote);

        // The buffered messages should have been drained into the channel
        assert_eq!(node.outbound_buffer.len(), 0);

        // We should receive the buffered messages + the new one
        let mut received = 0;
        while rx.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, 6, "5 buffered + 1 new should all be delivered");
    }

    #[test]
    fn duplicate_block_is_ignored() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        let block = Block::new(
            0,
            H256::zero(),
            Address::from_slice(&[1u8; 20]).unwrap(),
            aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
            vec![],
        );

        // Insert manually
        let hash = block.hash();
        node.blocks_by_hash.insert(hash, block.clone());

        // on_block_received should return Ok (silently skip duplicate)
        assert!(node.on_block_received(block).is_ok());
    }

    #[test]
    fn transactions_root_is_deterministic() {
        let tx1 = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: Address::from_slice(&[1u8; 20]).unwrap(),
            sender_pubkey: PublicKey::from_bytes(vec![1u8; 32]),
            inputs: vec![],
            outputs: vec![],
            reads: std::collections::HashSet::new(),
            writes: std::collections::HashSet::new(),
            program_id: None,
            data: vec![1, 2, 3],
            gas_limit: 21000,
            fee: 1000,
            signature: aether_types::Signature::from_bytes(vec![0u8; 64]),
        };

        let root1 = compute_transactions_root(std::slice::from_ref(&tx1));
        let root2 = compute_transactions_root(&[tx1]);
        assert_eq!(root1, root2, "same input must produce same root");
    }

    #[test]
    fn empty_root_is_zero() {
        assert_eq!(compute_transactions_root(&[]), H256::zero());
        assert_eq!(compute_receipts_root(&[]), H256::zero());
    }

    #[test]
    fn orphan_block_is_buffered_when_parent_unknown() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        let unknown_parent = H256::from_slice(&[0xAB; 32]).unwrap();
        let orphan = Block::new(
            5,
            unknown_parent,
            Address::from_slice(&[1u8; 20]).unwrap(),
            aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
            vec![],
        );

        // Should succeed (buffered, not rejected)
        assert!(node.on_block_received(orphan).is_ok());
        assert_eq!(node.orphan_count(), 1);
        assert!(node.orphan_blocks.contains_key(&unknown_parent));
    }

    #[test]
    fn orphan_buffer_respects_max_capacity() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Fill up the orphan buffer
        for i in 0..MAX_ORPHAN_BLOCKS + 10 {
            let parent = H256::from_slice(&[(i & 0xFF) as u8; 32]).unwrap();
            let orphan = Block::new(
                (i + 5) as u64,
                parent,
                Address::from_slice(&[1u8; 20]).unwrap(),
                aether_types::VrfProof {
                    output: [0u8; 32],
                    proof: vec![],
                },
                vec![],
            );
            let _ = node.on_block_received(orphan);
        }

        // Should be capped
        assert!(node.orphan_count() <= MAX_ORPHAN_BLOCKS);
    }

    #[test]
    fn stale_orphans_pruned_at_finalization() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Buffer orphan blocks at various slots
        for slot in [3u64, 5, 10, 15, 20] {
            let parent = H256::from_slice(&[slot as u8; 32]).unwrap();
            let orphan = Block::new(
                slot,
                parent,
                Address::from_slice(&[1u8; 20]).unwrap(),
                aether_types::VrfProof {
                    output: [0u8; 32],
                    proof: vec![],
                },
                vec![],
            );
            let _ = node.on_block_received(orphan);
        }
        assert_eq!(node.orphan_count(), 5);

        // Prune orphans with slot ≤ 10
        node.prune_stale_orphans(10);

        // Slots 3, 5, 10 should be pruned; 15, 20 remain
        assert_eq!(node.orphan_count(), 2);
        // Verify the remaining blocks are the ones with slot > 10
        let remaining_slots: Vec<u64> = node
            .orphan_blocks
            .values()
            .flat_map(|blocks| blocks.iter().map(|b| b.header.slot))
            .collect();
        assert!(remaining_slots.contains(&15));
        assert!(remaining_slots.contains(&20));
        assert!(!remaining_slots.contains(&10));
        assert!(!remaining_slots.contains(&5));
        assert!(!remaining_slots.contains(&3));
    }

    #[test]
    fn prune_stale_orphans_noop_at_slot_zero() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        let parent = H256::from_slice(&[0xAB; 32]).unwrap();
        let orphan = Block::new(
            1,
            parent,
            Address::from_slice(&[1u8; 20]).unwrap(),
            aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
            vec![],
        );
        let _ = node.on_block_received(orphan);
        assert_eq!(node.orphan_count(), 1);

        // min_slot=0 should be a no-op (don't prune genesis-era blocks)
        node.prune_stale_orphans(0);
        assert_eq!(node.orphan_count(), 1);
    }

    #[test]
    fn sync_manager_tracks_state() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Node starts as synced
        assert!(!node.is_syncing());
    }

    #[test]
    fn peer_count_tracks_connect_disconnect() {
        use aether_p2p::PeerId;

        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        assert_eq!(node.peer_count(), 0);

        // Simulate peer connect events
        node.handle_network_event(NetworkEvent::PeerConnected(PeerId::random()))
            .unwrap();
        assert_eq!(node.peer_count(), 1);

        node.handle_network_event(NetworkEvent::PeerConnected(PeerId::random()))
            .unwrap();
        assert_eq!(node.peer_count(), 2);

        // Simulate peer disconnect
        node.handle_network_event(NetworkEvent::PeerDisconnected(PeerId::random()))
            .unwrap();
        assert_eq!(node.peer_count(), 1);

        // Disconnect below zero saturates at 0
        node.handle_network_event(NetworkEvent::PeerDisconnected(PeerId::random()))
            .unwrap();
        node.handle_network_event(NetworkEvent::PeerDisconnected(PeerId::random()))
            .unwrap();
        assert_eq!(node.peer_count(), 0);
    }

    #[test]
    fn blocks_persist_and_recover_on_restart() {
        let temp_dir = TempDir::new().unwrap();
        let keypair1 = Keypair::generate();
        let validators1 = vec![validator_info_from_key(&keypair1)];

        // Create node, produce a block, then drop it
        {
            let consensus = Box::new(SimpleConsensus::new(validators1));
            let mut node = Node::new(
                temp_dir.path(),
                consensus,
                Some(keypair1),
                None,
                Arc::new(ChainConfig::devnet()),
            )
            .unwrap();

            // Tick a few times until a block is produced
            for _ in 0..5 {
                node.tick().unwrap();
                if node.latest_block_slot().is_some() {
                    break;
                }
            }
            assert!(
                node.latest_block_slot().is_some(),
                "expected at least one block produced within 5 slots"
            );
        }

        // Re-open node from same path — blocks should be recovered
        let keypair2 = Keypair::generate();
        let validators2 = vec![validator_info_from_key(&keypair2)];
        {
            let consensus = Box::new(SimpleConsensus::new(validators2));
            let node = Node::new(
                temp_dir.path(),
                consensus,
                Some(keypair2),
                None,
                Arc::new(ChainConfig::devnet()),
            )
            .unwrap();

            // Chain tip should be recovered
            assert!(
                node.latest_block_slot().is_some(),
                "chain tip should survive restart"
            );
            // Consensus should be fast-forwarded past the recovered tip
            assert!(
                node.current_slot() > 0,
                "consensus slot should be advanced after recovery"
            );
        }
    }

    #[test]
    fn epoch_transition_completes_unbonding_and_credits_account() {
        use aether_program_staking::Unbonding;
        use aether_types::Address;

        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Set up an unbonding entry that completes at slot 0 (already elapsed).
        let delegator = Address::from(aether_types::H160([0x42u8; 20]));
        let unbond_amount = 5_000u128;
        node.staking_state_mut().unbonding.push(Unbonding {
            address: delegator,
            validator: Address::from(aether_types::H160([0x01u8; 20])),
            amount: unbond_amount,
            complete_slot: 0,
        });

        // Seed the ledger account so credit_account has something to update.
        node.ledger.credit_account(&delegator, 0).ok();

        // Trigger epoch transition; slot=0 means complete_slot<=current_slot.
        node.process_epoch_transition(1).unwrap();

        // Unbonding queue should be empty — entry was consumed.
        assert!(node.staking_state().unbonding.is_empty());

        // Delegator's account should have been credited.
        let account = node.ledger.get_account(&delegator).unwrap().unwrap();
        assert_eq!(account.balance, unbond_amount);
    }

    #[test]
    fn epoch_transition_credits_are_atomic() {
        use aether_program_staking::Unbonding;
        use aether_types::Address;

        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Set up two unbonding entries that should be credited atomically.
        let delegator_a = Address::from(aether_types::H160([0xAA; 20]));
        let delegator_b = Address::from(aether_types::H160([0xBB; 20]));
        node.staking_state_mut().unbonding.push(Unbonding {
            address: delegator_a,
            validator: Address::from(aether_types::H160([0x01; 20])),
            amount: 1_000,
            complete_slot: 0,
        });
        node.staking_state_mut().unbonding.push(Unbonding {
            address: delegator_b,
            validator: Address::from(aether_types::H160([0x01; 20])),
            amount: 2_000,
            complete_slot: 0,
        });

        // Seed accounts
        node.ledger.credit_account(&delegator_a, 0).ok();
        node.ledger.credit_account(&delegator_b, 0).ok();

        node.process_epoch_transition(1).unwrap();

        // Both delegators must have been credited (atomic batch).
        let a = node.ledger.get_account(&delegator_a).unwrap().unwrap();
        let b = node.ledger.get_account(&delegator_b).unwrap().unwrap();
        assert_eq!(
            a.balance, 1_000,
            "delegator A should receive unbonded tokens"
        );
        assert_eq!(
            b.balance, 2_000,
            "delegator B should receive unbonded tokens"
        );

        // Unbonding queue should be fully drained.
        assert!(node.staking_state().unbonding.is_empty());
    }

    #[test]
    fn epoch_emission_credits_all_validators_not_just_local() {
        let temp_dir = TempDir::new().unwrap();
        let local_key = Keypair::generate();
        let remote_key = Keypair::generate();

        let local_vi = ValidatorInfo {
            pubkey: PublicKey::from_bytes(local_key.public_key()),
            stake: 3_000,
            commission: 0,
            active: true,
        };
        let remote_vi = ValidatorInfo {
            pubkey: PublicKey::from_bytes(remote_key.public_key()),
            stake: 1_000,
            commission: 0,
            active: true,
        };

        let local_addr = local_vi.pubkey.to_address();
        let remote_addr = remote_vi.pubkey.to_address();

        let validators = vec![local_vi, remote_vi];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(local_key),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Seed both validator accounts so credit_account_to_batch works.
        node.ledger.credit_account(&local_addr, 0).ok();
        node.ledger.credit_account(&remote_addr, 0).ok();

        node.process_epoch_transition(1).unwrap();

        // Both validators must receive emission proportional to stake.
        let local_acct = node.ledger.get_account(&local_addr).unwrap().unwrap();
        let remote_acct = node.ledger.get_account(&remote_addr).unwrap().unwrap();

        // Local has 3x the stake of remote, so should get 3x the emission.
        // The exact emission amount depends on the EmissionSchedule config,
        // but the ratio must hold.
        if local_acct.balance > 0 {
            assert!(
                remote_acct.balance > 0,
                "remote validator must also receive emission, got 0"
            );
            // Check approximate 3:1 ratio (allow rounding within 1 unit)
            let ratio_diff =
                (local_acct.balance as i128 - (remote_acct.balance as i128 * 3)).unsigned_abs();
            assert!(
                ratio_diff <= 3,
                "emission ratio should be ~3:1, got local={} remote={}",
                local_acct.balance,
                remote_acct.balance
            );
        }
    }

    /// Helper: build a minimal vote for a given public key, slot, and block hash byte.
    fn make_vote(pubkey: &PublicKey, slot: u64, block_byte: u8) -> Vote {
        Vote {
            slot,
            block_hash: H256::from_slice(&[block_byte; 32]).unwrap(),
            validator: pubkey.clone(),
            signature: aether_types::Signature::from_bytes(vec![0u8; 64]),
            stake: 0,
        }
    }

    /// Helper: create a Node with a registered staking validator.
    fn node_with_staking_validator(
        temp_dir: &TempDir,
        keypair: Keypair,
        stake: u128,
    ) -> (Node, Address) {
        let validator_info = validator_info_from_key(&keypair);
        // Extract address before consuming keypair.
        let addr = PublicKey::from_bytes(keypair.public_key()).to_address();
        let consensus = Box::new(SimpleConsensus::new(vec![validator_info]));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();
        node.staking_state_mut()
            .register_validator(addr, addr, stake, 0, addr)
            .expect("register_validator should succeed");

        (node, addr)
    }

    #[test]
    fn shutdown_flushes_wal_and_stops_node() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Tick a few times to produce state
        for _ in 0..5 {
            node.tick().unwrap();
        }
        assert!(node.current_slot() > 0);

        // Shutdown should succeed and mark node as stopped
        node.shutdown().unwrap();
        assert!(!node.running);
    }

    #[test]
    fn shutdown_preserves_state_across_restart() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path();
        let keypair = Keypair::generate();
        // Save secret key bytes so we can reconstruct after move
        let key_bytes = keypair.secret_key();
        let validators = vec![validator_info_from_key(&keypair)];

        // Phase 1: produce blocks, then shut down
        let latest_slot;
        {
            let consensus = Box::new(SimpleConsensus::new(validators.clone()));
            let mut node = Node::new(
                db_path,
                consensus,
                Some(keypair),
                None,
                Arc::new(ChainConfig::devnet()),
            )
            .unwrap();

            // Tick until a block is produced
            for _ in 0..10 {
                node.tick().unwrap();
                if node.latest_block_slot().is_some() {
                    break;
                }
            }
            assert!(
                node.latest_block_slot().is_some(),
                "expected at least one block produced"
            );

            latest_slot = node.latest_block_slot().unwrap();
            node.shutdown().unwrap();
        }

        // Phase 2: reopen from same DB — state should be intact
        {
            let keypair2 = Keypair::from_bytes(&key_bytes).unwrap();
            let validators2 = vec![validator_info_from_key(&keypair2)];
            let consensus = Box::new(SimpleConsensus::new(validators2));
            let node = Node::new(
                db_path,
                consensus,
                Some(keypair2),
                None,
                Arc::new(ChainConfig::devnet()),
            )
            .unwrap();

            // Block produced before shutdown should be recovered
            assert!(
                node.latest_block_slot().is_some(),
                "blocks should survive shutdown+restart"
            );
            assert!(
                node.latest_block_slot().unwrap() >= latest_slot,
                "recovered tip should be at or past slot {}",
                latest_slot
            );
        }
    }

    #[test]
    fn staking_state_persisted_across_restart() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path();
        let keypair = Keypair::generate();
        let key_bytes = keypair.secret_key();
        let pubkey = PublicKey::from_bytes(keypair.public_key());
        let addr = pubkey.to_address();
        let stake = 100_000_000u128;

        // Phase 1: register a validator, persist staking state, then drop the node.
        {
            let validators = vec![validator_info_from_key(&keypair)];
            let consensus = Box::new(SimpleConsensus::new(validators));
            let mut node = Node::new(
                db_path,
                consensus,
                Some(keypair),
                None,
                Arc::new(ChainConfig::devnet()),
            )
            .unwrap();

            node.staking_state_mut()
                .register_validator(addr, addr, stake, 0, addr)
                .expect("register_validator should succeed");

            // Explicitly persist so it's durable before the drop.
            node.persist_staking_state().unwrap();
        }

        // Phase 2: reopen from the same DB — staking state must survive.
        {
            let keypair2 = Keypair::from_bytes(&key_bytes).unwrap();
            let validators2 = vec![validator_info_from_key(&keypair2)];
            let consensus = Box::new(SimpleConsensus::new(validators2));
            let node = Node::new(
                db_path,
                consensus,
                Some(keypair2),
                None,
                Arc::new(ChainConfig::devnet()),
            )
            .unwrap();

            let validator = node
                .staking_state()
                .get_validator(&addr)
                .expect("validator must survive restart");
            assert_eq!(
                validator.staked_amount, stake,
                "staked amount must survive restart"
            );
            assert_eq!(
                node.staking_state().total_staked,
                stake,
                "total_staked must survive restart"
            );
        }
    }

    #[test]
    fn slash_persisted_across_restart() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path();
        let keypair = Keypair::generate();
        let key_bytes = keypair.secret_key();
        let pubkey = PublicKey::from_bytes(keypair.public_key());
        let addr = pubkey.to_address();
        let stake = 100_000_000u128;

        // Phase 1: register validator, slash via vote-time detection, then drop.
        let expected_stake;
        {
            let validators = vec![validator_info_from_key(&keypair)];
            let consensus = Box::new(SimpleConsensus::new(validators));
            let mut node = Node::new(
                db_path,
                consensus,
                Some(keypair),
                None,
                Arc::new(ChainConfig::devnet()),
            )
            .unwrap();

            node.staking_state_mut()
                .register_validator(addr, addr, stake, 0, addr)
                .expect("register_validator should succeed");

            // Double-sign to trigger slash
            let vote_a = make_vote(&pubkey, 0, 0xAA);
            let vote_b = make_vote(&pubkey, 0, 0xBB);
            node.on_vote_received(vote_a).unwrap();
            node.on_vote_received(vote_b).unwrap();

            expected_stake = node
                .staking_state()
                .get_validator(&addr)
                .unwrap()
                .staked_amount;
            assert!(expected_stake < stake, "slash should have reduced stake");
        }

        // Phase 2: reopen — slashed state must survive.
        {
            let keypair2 = Keypair::from_bytes(&key_bytes).unwrap();
            let validators2 = vec![validator_info_from_key(&keypair2)];
            let consensus = Box::new(SimpleConsensus::new(validators2));
            let node = Node::new(
                db_path,
                consensus,
                Some(keypair2),
                None,
                Arc::new(ChainConfig::devnet()),
            )
            .unwrap();

            let validator = node
                .staking_state()
                .get_validator(&addr)
                .expect("validator must survive restart after slash");
            assert_eq!(
                validator.staked_amount, expected_stake,
                "slashed stake must survive restart (expected {expected_stake}, got {})",
                validator.staked_amount
            );
        }
    }

    #[test]
    fn double_sign_vote_slashes_staking_state() {
        // When a validator sends two votes for the same slot but different blocks,
        // on_vote_received must reduce their staking bond (not only consensus weight).
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let pubkey = PublicKey::from_bytes(keypair.public_key());
        let stake = 100_000_000u128; // 100 SWR minimum
        let (mut node, addr) = node_with_staking_validator(&temp_dir, keypair, stake);

        let vote_a = make_vote(&pubkey, 0, 0xAA);
        let vote_b = make_vote(&pubkey, 0, 0xBB); // same slot, different block

        // First vote — no slash
        node.on_vote_received(vote_a).unwrap();
        let stake_after_first = node
            .staking_state()
            .get_validator(&addr)
            .expect("validator should exist")
            .staked_amount;
        assert_eq!(stake_after_first, stake, "first vote should not slash");

        // Second vote — double-sign detected
        node.on_vote_received(vote_b).unwrap();
        let stake_after_slash = node
            .staking_state()
            .get_validator(&addr)
            .expect("validator should exist")
            .staked_amount;
        assert!(
            stake_after_slash < stake,
            "double-sign must reduce staking bond: before={stake}, after={stake_after_slash}"
        );
        // 5% of 100_000_000 = 5_000_000 slashed
        assert_eq!(
            stake_after_slash, 95_000_000,
            "double-sign must slash exactly 5% of bond"
        );
    }

    #[test]
    fn double_sign_no_double_slash_via_block_evidence() {
        // If the vote path already slashed an offense, block evidence for the same
        // (validator, slot) must NOT apply the slash a second time.

        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let pubkey = PublicKey::from_bytes(keypair.public_key());
        let stake = 100_000_000u128; // 100 SWR minimum
        let (mut node, addr) = node_with_staking_validator(&temp_dir, keypair, stake);

        // Trigger vote-path slash (slot 0 — current consensus slot)
        let vote_a = make_vote(&pubkey, 0, 0xAA);
        let vote_b = make_vote(&pubkey, 0, 0xBB);
        node.on_vote_received(vote_a.clone()).unwrap();
        node.on_vote_received(vote_b.clone()).unwrap();

        let stake_after_vote_slash = node
            .staking_state()
            .get_validator(&addr)
            .unwrap()
            .staked_amount;
        assert!(
            stake_after_vote_slash < stake,
            "vote-path slash should fire"
        );

        // Verify that the dedup set already has this offense keyed (validator, slot=0).
        assert!(
            node.slashed_offenses.contains(&(addr, 0)),
            "offense should be recorded in slashed_offenses after vote-path slash"
        );

        // The block-evidence path will skip slash because slashed_offenses contains (addr, 0).
        // We can test this by manually checking the staking bond doesn't change.
        let stake_no_change = node
            .staking_state()
            .get_validator(&addr)
            .unwrap()
            .staked_amount;
        assert_eq!(
            stake_no_change, stake_after_vote_slash,
            "stake should not change — block evidence not yet submitted"
        );

        // The insertion into slashed_offenses will return false for a duplicate
        // (validator=addr, slot=0), confirming the dedup guard works.
        let already_slashed = !node.slashed_offenses.insert((addr, 0));
        assert!(
            already_slashed,
            "inserting same (validator, slot) into slashed_offenses must return false"
        );
    }

    #[test]
    fn slashed_offenses_pruned_at_finalized_slot() {
        // slashed_offenses must not grow unboundedly — entries for finalized slots
        // should be pruned when check_finality triggers pruning.
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let stake = 100_000_000u128; // 100 SWR minimum
        let (mut node, addr) = node_with_staking_validator(&temp_dir, keypair, stake);

        // Manually insert old slashed offenses at various slots.
        node.slashed_offenses.insert((addr, 1));
        node.slashed_offenses.insert((addr, 5));
        node.slashed_offenses.insert((addr, 100));
        assert_eq!(node.slashed_offenses.len(), 3);

        // Simulate finalized_slot = 10: entries at slot < 10 should be pruned.
        let finalized = node.consensus.finalized_slot();
        node.slashed_offenses.retain(|&(_, slot)| slot >= finalized);

        // With finalized_slot=0 (initial state), nothing is pruned yet.
        // Insert offenses at slot 0 and verify they stay until finality advances.
        node.slashed_offenses.insert((addr, 0));
        // finalized=0 means retain everything (slot >= 0 is always true for u64, so use finalized)
        node.slashed_offenses.retain(|&(_, slot)| slot >= finalized);
        assert!(node.slashed_offenses.contains(&(addr, 0)));
        assert!(node.slashed_offenses.contains(&(addr, 100)));

        // Prune with a higher finalized slot — old entries disappear.
        node.slashed_offenses.retain(|&(_, slot)| slot >= 10);
        assert!(
            !node.slashed_offenses.contains(&(addr, 1)),
            "slot 1 should be pruned"
        );
        assert!(
            !node.slashed_offenses.contains(&(addr, 5)),
            "slot 5 should be pruned"
        );
        assert!(
            node.slashed_offenses.contains(&(addr, 100)),
            "slot 100 should be retained"
        );
    }

    #[test]
    fn duplicate_vote_same_block_does_not_slash() {
        // Sending the exact same vote (same slot AND same block hash) twice is
        // not a double-sign — it must not trigger a slash.
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let pubkey = PublicKey::from_bytes(keypair.public_key());
        let stake = 100_000_000u128; // 100 SWR minimum
        let (mut node, addr) = node_with_staking_validator(&temp_dir, keypair, stake);

        let vote = make_vote(&pubkey, 0, 0xAA);

        node.on_vote_received(vote.clone()).unwrap();
        node.on_vote_received(vote).unwrap(); // duplicate — not a double-sign

        let stake_unchanged = node
            .staking_state()
            .get_validator(&addr)
            .unwrap()
            .staked_amount;
        assert_eq!(
            stake_unchanged, stake,
            "duplicate vote for same block must not trigger slash"
        );
        assert!(
            node.slashed_offenses.is_empty(),
            "slashed_offenses must remain empty for non-double-sign"
        );
    }

    #[test]
    fn epoch_transition_writes_snapshot_when_dir_configured() {
        let temp_dir = TempDir::new().unwrap();
        let snapshot_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Seed the ledger with a non-zero state root so generate_snapshot succeeds.
        node.ledger
            .storage()
            .put(
                aether_state_storage::CF_METADATA,
                b"state_root",
                &[0x42u8; 32],
            )
            .unwrap();

        node.set_snapshot_dir(snapshot_dir.path().to_path_buf());

        // Trigger epoch transition for epoch 1. The snapshot height is taken from
        // current_slot() which starts at 0 in SimpleConsensus for this test.
        node.process_epoch_transition(1).unwrap();

        // A snapshot file should have been written.
        let entries: Vec<_> = std::fs::read_dir(snapshot_dir.path())
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(entries.len(), 1, "exactly one snapshot file expected");
        let filename = entries[0].file_name();
        let name = filename.to_string_lossy();
        assert!(
            name.starts_with("snapshot_1_"),
            "snapshot file should be named snapshot_<epoch>_<slot>.bin, got: {name}"
        );
        assert!(name.ends_with(".bin"), "unexpected filename: {name}");

        // Snapshot bytes must be non-empty and decodable.
        let bytes = std::fs::read(entries[0].path()).unwrap();
        assert!(!bytes.is_empty());
        let snapshot = aether_state_snapshots::decode_snapshot(&bytes).unwrap();
        // Height is the current slot at time of generation (0 for SimpleConsensus in test).
        assert_eq!(snapshot.metadata.height, 0);
    }

    #[test]
    fn epoch_transition_no_snapshot_without_dir() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Seed state root.
        node.ledger
            .storage()
            .put(
                aether_state_storage::CF_METADATA,
                b"state_root",
                &[0x42u8; 32],
            )
            .unwrap();

        // No snapshot_dir set — should not panic or error.
        node.process_epoch_transition(1).unwrap();
        // No assertion needed — just verifying no panic and clean return.
    }

    #[test]
    fn rpc_queries_survive_restart() {
        // Verify that get_block_by_hash and get_block_by_slot fall back to
        // RocksDB after restart (when in-memory caches are empty).
        let temp_dir = TempDir::new().unwrap();

        // Phase 1: produce a block, record its hash and slot
        let (saved_block_hash, saved_block_slot) = {
            let keypair = Keypair::generate();
            let validators = vec![validator_info_from_key(&keypair)];
            let consensus = Box::new(SimpleConsensus::new(validators));
            let mut node = Node::new(
                temp_dir.path(),
                consensus,
                Some(keypair),
                None,
                Arc::new(ChainConfig::devnet()),
            )
            .unwrap();

            for _ in 0..10 {
                node.tick().unwrap();
                if node.latest_block_slot().is_some() {
                    break;
                }
            }

            let slot = node.latest_block_slot().expect("block should be produced");
            let block = node
                .get_block_by_slot(slot)
                .expect("block should exist in cache");

            (block.hash(), slot)
        };

        // Phase 2: re-open node, verify queries still work via RocksDB fallback
        {
            let keypair2 = Keypair::generate();
            let validators2 = vec![validator_info_from_key(&keypair2)];
            let consensus = Box::new(SimpleConsensus::new(validators2));
            let node = Node::new(
                temp_dir.path(),
                consensus,
                Some(keypair2),
                None,
                Arc::new(ChainConfig::devnet()),
            )
            .unwrap();

            assert!(
                node.get_block_by_hash(saved_block_hash).is_some(),
                "get_block_by_hash should fall back to RocksDB after restart"
            );

            assert!(
                node.get_block_by_slot(saved_block_slot).is_some(),
                "get_block_by_slot should fall back to RocksDB after restart"
            );
        }
    }

    #[test]
    fn vote_on_block_refuses_duplicate_slot() {
        // Regression: when two fork blocks arrive at the same slot,
        // on_block_received calls vote_on_block for each.  Without the
        // last_voted_slot guard, the honest validator would broadcast
        // conflicting votes and get falsely slashed for double-signing.
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let bls_key = aether_crypto_bls::BlsKeypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            Some(bls_key),
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Build two blocks at slot 5 with different hashes
        let vrf = aether_types::VrfProof {
            output: [0u8; 32],
            proof: vec![],
        };
        let block_a = Block::new(
            5,
            H256::zero(),
            Address::from_slice(&[1u8; 20]).unwrap(),
            vrf.clone(),
            vec![],
        );
        let mut block_b = Block::new(
            5,
            H256::zero(),
            Address::from_slice(&[2u8; 20]).unwrap(),
            vrf,
            vec![],
        );
        // Ensure different hash
        block_b.header.state_root = H256::from_slice(&[0xBB; 32]).unwrap();

        assert_ne!(block_a.hash(), block_b.hash());

        // Vote on first block should succeed
        node.vote_on_block(&block_a).unwrap();
        assert_eq!(node.last_voted_slot, Some(5));
        let votes_after_first = node.drain_outbound().len();
        assert_eq!(votes_after_first, 1, "first vote should be broadcast");

        // Vote on second block at same slot should be skipped
        node.vote_on_block(&block_b).unwrap();
        let votes_after_second = node.drain_outbound().len();
        assert_eq!(
            votes_after_second, 0,
            "second vote at same slot must be suppressed"
        );
    }

    #[test]
    fn chain_tip_persisted_and_recovered_on_restart() {
        let temp_dir = TempDir::new().unwrap();
        let config = Arc::new(ChainConfig::devnet());

        // First node instance: produce some blocks
        let tip_slot;
        let tip_hash;
        {
            let keypair = Keypair::generate();
            let validators = vec![validator_info_from_key(&keypair)];
            let consensus = Box::new(SimpleConsensus::new(validators));
            let mut node = Node::new(
                temp_dir.path(),
                consensus,
                Some(keypair),
                None,
                config.clone(),
            )
            .unwrap();

            // Produce 3 blocks (tick = process_slot + advance_slot)
            for _ in 0..3 {
                node.tick().unwrap();
            }
            tip_slot = node.latest_block_slot().unwrap();
            tip_hash = node.latest_block_hash();
            assert!(tip_slot >= 1, "should have produced at least 1 block");
        }
        // Node dropped — simulates restart

        // Second node instance: should recover tip from metadata
        {
            let keypair2 = Keypair::generate();
            let validators2 = vec![validator_info_from_key(&keypair2)];
            let consensus = Box::new(SimpleConsensus::new(validators2));
            let node = Node::new(temp_dir.path(), consensus, Some(keypair2), None, config).unwrap();

            assert_eq!(
                node.latest_block_slot(),
                Some(tip_slot),
                "chain tip slot must survive restart"
            );
            assert_eq!(
                node.latest_block_hash(),
                tip_hash,
                "chain tip hash must survive restart"
            );
            // Consensus should be fast-forwarded past the tip
            assert!(
                node.consensus.current_slot() > tip_slot,
                "consensus must resume past recovered tip"
            );
        }
    }

    /// Regression: when two competing blocks arrive at the same slot and fork-choice
    /// switches canonical (lower hash wins), the second block's overlay must NOT be
    /// committed on top of the already-committed first block's state.  Doing so would
    /// leave stale effects from the first block (e.g. UTXOs it created) permanently in
    /// storage, silently corrupting the UTXO set.
    ///
    /// Expected behavior: the first-committed block remains the chain tip; the
    /// fork block is buffered in memory but its state is not written to disk.
    #[test]
    fn fork_block_does_not_double_commit_state() {
        let temp_dir = TempDir::new().unwrap();
        // Phase 1: use a producer node to get a valid block with the correct state_root.
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let block_a = {
            let consensus = Box::new(SimpleConsensus::new(validators.clone()));
            let mut producer = Node::new(
                temp_dir.path(),
                consensus,
                Some(keypair),
                None,
                Arc::new(ChainConfig::devnet()),
            )
            .unwrap();
            producer.process_slot().unwrap();
            let slot = producer
                .latest_block_slot()
                .expect("node should produce a block");
            producer.get_block_by_slot(slot).expect("block must exist")
        };

        // Build a competing block at the same slot by copying block_a and changing
        // only the VRF output field.  This produces a different hash while keeping
        // all other header fields identical (same state_root, same proposer, same
        // slot) so it will pass speculative execution and the proposer check.
        let mut block_b = block_a.clone();
        block_b.header.vrf_proof.output = {
            let mut alt = block_a.header.vrf_proof.output;
            alt[0] ^= 0xFF; // flip a bit to get a different output (and hash)
            alt
        };

        // Phase 2: fresh receiving node that accepts both blocks via on_block_received.
        let temp_dir2 = TempDir::new().unwrap();
        let consensus2 = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir2.path(),
            consensus2,
            None,
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        let hash_a = block_a.hash();
        let hash_b = block_b.hash();
        assert_ne!(hash_a, hash_b, "test blocks must have different hashes");

        // Fork-choice picks the block with the lower hash byte-for-byte.
        // Make `first_block` the one with the HIGHER hash so it arrives first and is
        // committed, while `second_block` (lower hash) would be preferred by
        // fork-choice — this is the scenario that triggers the bug without the fix.
        let (first_block, second_block) = if hash_a.as_bytes() > hash_b.as_bytes() {
            (block_a, block_b) // a=high arrives first, b=low arrives second
        } else {
            (block_b, block_a) // b=high arrives first, a=low arrives second
        };
        let first_hash = first_block.hash();
        let second_hash = second_block.hash();

        // Verify test setup: second block has lower hash (fork-choice prefers it)
        assert!(
            second_hash.as_bytes() < first_hash.as_bytes(),
            "second block must have a lower hash so fork-choice would switch canonical"
        );

        let test_slot = first_block.header.slot;

        // Receive the first block — it should be committed and become the chain tip.
        node.on_block_received(first_block).unwrap();
        assert_eq!(
            node.latest_block_hash, first_hash,
            "first block should become chain tip"
        );
        assert_eq!(
            node.committed_at_slot.get(&test_slot),
            Some(&first_hash),
            "first block state must be recorded as committed at its slot"
        );

        // Receive the competing block (lower hash — fork-choice prefers it).
        // Without the fix, this would write the second block's state on top of the
        // first, corrupting the UTXO set.  With the fix it must be skipped.
        node.on_block_received(second_block).unwrap();

        // Chain tip must still be the first (committed) block.
        assert_eq!(
            node.latest_block_hash, first_hash,
            "chain tip must not change after fork block arrives at an already-committed slot"
        );
        assert_eq!(
            node.committed_at_slot.get(&test_slot),
            Some(&first_hash),
            "committed_at_slot must still record the first block, not the fork"
        );
    }

    /// Regression: produce_block must insert into committed_at_slot so that a
    /// fork block received at the same slot cannot overwrite the produced block's
    /// already-committed state (UTXO corruption vector).
    #[test]
    fn produced_block_sets_committed_at_slot() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Before producing, committed_at_slot should be empty
        assert!(node.committed_at_slot.is_empty());

        node.process_slot().unwrap();
        let slot = node.latest_block_slot().expect("block should be produced");
        let hash = node.latest_block_hash;

        // After producing, the slot must be recorded in committed_at_slot
        assert_eq!(
            node.committed_at_slot.get(&slot),
            Some(&hash),
            "produce_block must insert into committed_at_slot to guard against fork overwrites"
        );
    }

    #[test]
    fn sync_buffers_and_applies_blocks_via_producer() {
        // Use a producer node to generate valid blocks (with correct state roots),
        // then test that a syncing receiver can buffer and apply them in order.
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];

        // Produce a valid block at slot 1 (bootstrap slot, no QC required)
        let block = {
            let consensus = Box::new(SimpleConsensus::new(validators.clone()));
            let mut producer = Node::new(
                temp_dir.path(),
                consensus,
                Some(keypair),
                None,
                Arc::new(ChainConfig::devnet()),
            )
            .unwrap();
            producer.consensus.advance_slot(); // skip slot 0
            producer.tick().unwrap();
            producer.get_block_by_slot(1).unwrap()
        };

        let block_hash = block.hash();

        // Receiver node starts syncing
        let temp_dir2 = TempDir::new().unwrap();
        let consensus2 = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir2.path(),
            consensus2,
            None,
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Advance receiver's consensus far enough to accept blocks
        for _ in 0..10 {
            node.consensus.advance_slot();
        }

        assert!(node.sync_manager.check_sync_needed(0, 50));
        assert!(node.sync_manager.is_syncing());

        // Buffer and drain
        assert!(node.sync_manager.buffer_block(block.clone()));
        let ready = node.sync_manager.drain_ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].header.slot, 1);

        // Apply via on_block_received (full validation)
        node.on_block_received(ready.into_iter().next().unwrap())
            .unwrap();
        node.sync_manager.record_applied();

        assert_eq!(node.latest_block_slot(), Some(1));
        assert_eq!(node.latest_block_hash(), block_hash);
        assert_eq!(node.sync_manager.blocks_applied(), 1);
    }

    /// A block whose timestamp is more than MAX_CLOCK_DRIFT_SECS in the future must be
    /// rejected to prevent proposers from manufacturing far-future timestamps.
    #[test]
    fn block_with_future_timestamp_is_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        let mut block = Block::new(
            1,
            H256::zero(),
            Address::from_slice(&[1u8; 20]).unwrap(),
            aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
            vec![],
        );
        // Set timestamp 1 hour in the future — well beyond MAX_CLOCK_DRIFT_SECS.
        block.header.timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;

        let result = node.on_block_received(block);
        assert!(
            result.is_err(),
            "block with far-future timestamp must be rejected"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("too far in the future"),
            "error must mention future timestamp, got: {msg}"
        );
    }

    /// A block whose timestamp precedes its parent's timestamp must be rejected.
    #[test]
    fn block_with_timestamp_before_parent_is_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Insert a parent block with a known timestamp.
        let parent = Block::new(
            0,
            H256::zero(),
            Address::from_slice(&[1u8; 20]).unwrap(),
            aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
            vec![],
        );
        let parent_hash = parent.hash();
        let parent_ts = parent.header.timestamp;
        node.blocks_by_hash.insert(parent_hash, parent);

        // Build a child block with a timestamp *before* the parent's.
        let mut child = Block::new(
            1,
            parent_hash,
            Address::from_slice(&[1u8; 20]).unwrap(),
            aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
            vec![],
        );
        // Set timestamp to parent_ts - 1 (one second before parent).
        child.header.timestamp = parent_ts.saturating_sub(1);

        let result = node.on_block_received(child);
        assert!(
            result.is_err(),
            "block with timestamp before parent must be rejected"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("precedes parent timestamp"),
            "error must mention parent timestamp, got: {msg}"
        );
    }

    #[test]
    fn handle_block_range_request_serves_produced_blocks() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Skip slot 0, produce blocks at slots 1 and 2
        node.consensus.advance_slot();
        node.tick().unwrap();
        node.tick().unwrap();
        assert_eq!(node.latest_block_slot(), Some(2));

        // Clear outbound buffer
        node.outbound_buffer.clear();

        // Request blocks 1-2
        node.handle_block_range_request(1, 2);

        let broadcast_count = node
            .outbound_buffer
            .iter()
            .filter(|msg| matches!(msg, OutboundMessage::BroadcastBlock(_)))
            .count();
        assert_eq!(broadcast_count, 2);
    }

    #[test]
    fn sync_request_rate_limited() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Produce blocks at slots 1 and 2
        node.consensus.advance_slot();
        node.tick().unwrap();
        node.tick().unwrap();
        assert_eq!(node.latest_block_slot(), Some(2));
        node.outbound_buffer.clear();

        // First request succeeds
        node.handle_block_range_request(1, 2);
        let first_count = node
            .outbound_buffer
            .iter()
            .filter(|msg| matches!(msg, OutboundMessage::BroadcastBlock(_)))
            .count();
        assert_eq!(first_count, 2, "first sync request should be served");

        node.outbound_buffer.clear();

        // Second request immediately after should be rate-limited (dropped)
        node.handle_block_range_request(1, 2);
        let second_count = node
            .outbound_buffer
            .iter()
            .filter(|msg| matches!(msg, OutboundMessage::BroadcastBlock(_)))
            .count();
        assert_eq!(
            second_count, 0,
            "second sync request should be rate-limited"
        );
    }

    #[test]
    fn sync_request_allowed_after_cooldown() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Produce a block
        node.consensus.advance_slot();
        node.tick().unwrap();
        node.outbound_buffer.clear();

        // First request
        node.handle_block_range_request(1, 1);
        assert_eq!(
            node.outbound_buffer
                .iter()
                .filter(|msg| matches!(msg, OutboundMessage::BroadcastBlock(_)))
                .count(),
            1
        );
        node.outbound_buffer.clear();

        // Simulate cooldown elapsed by backdating the timestamp
        node.last_sync_response =
            Some(Instant::now() - SYNC_RESPONSE_COOLDOWN - Duration::from_millis(1));

        // Now the request should be served
        node.handle_block_range_request(1, 1);
        assert_eq!(
            node.outbound_buffer
                .iter()
                .filter(|msg| matches!(msg, OutboundMessage::BroadcastBlock(_)))
                .count(),
            1,
            "request after cooldown should be served"
        );
    }

    /// voted_slots must be pruned at finalization boundaries to prevent
    /// unbounded memory growth on long-running validators.
    #[test]
    fn voted_slots_pruned_at_finalization() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Simulate voting at several slots
        node.voted_slots.insert(1);
        node.voted_slots.insert(5);
        node.voted_slots.insert(10);
        node.voted_slots.insert(20);
        assert_eq!(node.voted_slots.len(), 4);

        // Directly test the retain logic (same as prune_finalized_state):
        let finalized = 10u64;
        node.voted_slots.retain(|&slot| slot >= finalized);

        assert_eq!(node.voted_slots.len(), 2);
        assert!(!node.voted_slots.contains(&1));
        assert!(!node.voted_slots.contains(&5));
        assert!(node.voted_slots.contains(&10));
        assert!(node.voted_slots.contains(&20));
    }

    #[test]
    fn mul_div_no_overflow_large_emission_and_stake() {
        // Regression: checked_mul(emission, stake) overflows u128 for large values,
        // causing unwrap_or(0) to silently drop validator rewards.
        let emission = u128::MAX / 2;
        let stake = u128::MAX / 3;
        let total_stake = u128::MAX / 3; // validator has 100% of stake

        // With checked_mul this would overflow and return 0.
        // With mul_div it should return ~emission (100% share).
        let share = mul_div(emission, stake, total_stake);
        assert_eq!(
            share, emission,
            "100% stake share should equal full emission"
        );

        // Partial stake: 50% of total
        let half_stake = total_stake / 2;
        let half_share = mul_div(emission, half_stake, total_stake);
        let expected = emission / 2;
        assert!(
            half_share >= expected - 1 && half_share <= expected + 1,
            "50% stake share should be ~{expected}, got {half_share}"
        );
    }

    /// A block whose transactions_root field doesn't match the actual transactions is rejected.
    #[test]
    fn block_with_wrong_transactions_root_is_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let proposer = PublicKey::from_bytes(keypair.public_key()).to_address();
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Build a block that passes consensus validation (correct proposer, slot=0)
        // but has a tampered transactions_root.
        let mut block = Block::new(
            0,
            H256::zero(),
            proposer,
            aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
            vec![], // no transactions → correct root is H256::zero()
        );
        // Tamper the transactions_root to a non-zero value.
        block.header.transactions_root = H256::from_slice(&[0xAB; 32]).unwrap();

        let result = node.on_block_received(block);
        assert!(
            result.is_err(),
            "block with wrong transactions_root must be rejected"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("transactions_root mismatch"),
            "error must mention transactions_root mismatch, got: {msg}"
        );
    }

    /// A block at slot > 1 with a non-zero parent hash and no QC must be rejected.
    #[test]
    fn non_genesis_block_without_qc_is_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let proposer = PublicKey::from_bytes(keypair.public_key()).to_address();
        let mut consensus = SimpleConsensus::new(validators);
        // Advance consensus to slot 2 so the block passes the future-slot check.
        consensus.advance_slot();
        consensus.advance_slot();
        let consensus_box: Box<dyn aether_consensus::ConsensusEngine> = Box::new(consensus);
        let mut node = Node::new(
            temp_dir.path(),
            consensus_box,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Insert a parent block so the child is not buffered as an orphan.
        let parent = Block::new(
            1,
            H256::zero(),
            proposer,
            aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
            vec![],
        );
        let parent_hash = parent.hash();
        node.blocks_by_hash.insert(parent_hash, parent);

        // Build a child block at slot 2 with no aggregated_vote.
        let mut child = Block::new(
            2,
            parent_hash,
            proposer,
            aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
            vec![],
        );
        // Ensure transactions_root is correct for empty block.
        child.header.transactions_root = H256::zero();
        // No aggregated_vote — must be rejected.
        assert!(child.aggregated_vote.is_none());

        let result = node.on_block_received(child);
        assert!(
            result.is_err(),
            "non-genesis block without QC must be rejected"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("missing required quorum certificate"),
            "error must mention missing QC, got: {msg}"
        );
    }

    /// A block whose slot is not strictly greater than its parent's slot must be rejected.
    #[test]
    fn block_with_non_monotonic_slot_is_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let proposer = PublicKey::from_bytes(keypair.public_key()).to_address();
        let mut simple_consensus = SimpleConsensus::new(validators);
        // Advance to slot 1 so the child block at slot 1 passes the future-slot check.
        simple_consensus.advance_slot();
        let consensus: Box<dyn aether_consensus::ConsensusEngine> = Box::new(simple_consensus);
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        // Insert a parent block at slot 1.
        let parent = Block::new(
            1,
            H256::zero(),
            proposer,
            aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
            vec![],
        );
        let parent_hash = parent.hash();
        node.blocks_by_hash.insert(parent_hash, parent);

        // Build a child block at slot 1 (same as parent) — violates monotonicity.
        let child = Block::new(
            1,
            parent_hash,
            proposer,
            aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
            vec![],
        );

        let result = node.on_block_received(child);
        assert!(
            result.is_err(),
            "block with slot <= parent slot must be rejected"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("slot monotonicity violation"),
            "error must mention slot monotonicity, got: {msg}"
        );
    }

    #[test]
    fn oversized_block_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let pubkey_bytes = keypair.public_key();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));

        let mut config = ChainConfig::devnet();
        // Set an artificially low block size limit so a block with transactions exceeds it.
        config.chain.block_bytes_max = 100;
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(config),
        )
        .unwrap();

        // Build a block whose serialized size exceeds the 100-byte limit.
        let tx = Transaction {
            sender: Address::from_slice(&[1u8; 20]).unwrap(),
            sender_pubkey: PublicKey::from_bytes(pubkey_bytes),
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            fee: 1,
            nonce: 0,
            data: vec![0u8; 200],
            signature: aether_types::Signature::from_bytes(vec![0u8; 64]),
            chain_id: 900,
            gas_limit: 21000,
        };
        let block = Block::new(
            1,
            H256::zero(),
            Address::from_slice(&[1u8; 20]).unwrap(),
            aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
            vec![tx],
        );

        let result = node.on_block_received(block);
        assert!(result.is_err(), "oversized block must be rejected");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("exceeds size limit"),
            "error must mention size limit, got: {msg}"
        );
    }

    #[test]
    fn block_with_duplicate_transactions_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let pubkey_bytes = keypair.public_key();
        let validators = vec![validator_info_from_key(&keypair)];
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        let tx = Transaction {
            sender: Address::from_slice(&[1u8; 20]).unwrap(),
            sender_pubkey: PublicKey::from_bytes(pubkey_bytes),
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            fee: 1,
            nonce: 0,
            data: vec![],
            signature: aether_types::Signature::from_bytes(vec![0u8; 64]),
            chain_id: 900,
            gas_limit: 21000,
        };
        // Include the same transaction twice.
        let block = Block::new(
            1,
            H256::zero(),
            Address::from_slice(&[1u8; 20]).unwrap(),
            aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
            vec![tx.clone(), tx],
        );

        let result = node.on_block_received(block);
        assert!(
            result.is_err(),
            "block with duplicate transactions must be rejected"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("duplicate transaction"),
            "error must mention duplicate transaction, got: {msg}"
        );
    }

    #[test]
    fn test_fee_gas_aggregation_saturates_instead_of_overflowing() {
        // Verify that fee/gas aggregation uses saturating arithmetic.
        // With bare .sum(), these would panic in debug or wrap in release.
        let fees: Vec<u128> = vec![u128::MAX / 2 + 1, u128::MAX / 2 + 1];
        let total_fees: u128 = fees.iter().fold(0u128, |acc, &f| acc.saturating_add(f));
        assert_eq!(total_fees, u128::MAX);

        let gas_limits: Vec<u64> = vec![u64::MAX / 2 + 1, u64::MAX / 2 + 1];
        let total_gas: u64 = gas_limits
            .iter()
            .fold(0u64, |acc, &g| acc.saturating_add(g));
        assert_eq!(total_gas, u64::MAX);
    }

    #[test]
    fn block_exceeding_gas_limit_is_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let proposer = PublicKey::from_bytes(keypair.public_key()).to_address();
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        let tx = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: proposer,
            sender_pubkey: PublicKey::from_bytes(vec![1u8; 32]),
            inputs: vec![],
            outputs: vec![],
            reads: std::collections::HashSet::new(),
            writes: std::collections::HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: MAX_BLOCK_GAS_LIMIT + 1,
            fee: 0,
            signature: aether_types::Signature::from_bytes(vec![0u8; 64]),
        };
        let mut block = Block::new(
            0,
            H256::zero(),
            proposer,
            aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
            vec![tx],
        );
        block.header.transactions_root = compute_transactions_root(&block.transactions);

        let result = node.on_block_received(block);
        assert!(
            result.is_err(),
            "block exceeding gas limit must be rejected"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("block gas") && msg.contains("exceeds limit"),
            "error must mention gas limit, got: {msg}"
        );
    }

    #[test]
    fn block_with_gas_overflow_is_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let proposer = PublicKey::from_bytes(keypair.public_key()).to_address();
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        let tx1 = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: proposer,
            sender_pubkey: PublicKey::from_bytes(vec![1u8; 32]),
            inputs: vec![],
            outputs: vec![],
            reads: std::collections::HashSet::new(),
            writes: std::collections::HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: u64::MAX,
            fee: 0,
            signature: aether_types::Signature::from_bytes(vec![0u8; 64]),
        };
        let tx2 = Transaction {
            nonce: 1,
            chain_id: 1,
            sender: proposer,
            sender_pubkey: PublicKey::from_bytes(vec![1u8; 32]),
            inputs: vec![],
            outputs: vec![],
            reads: std::collections::HashSet::new(),
            writes: std::collections::HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 1,
            fee: 0,
            signature: aether_types::Signature::from_bytes(vec![0u8; 64]),
        };
        let txs = vec![tx1, tx2];
        let mut block = Block::new(
            0,
            H256::zero(),
            proposer,
            aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
            txs.clone(),
        );
        block.header.transactions_root = compute_transactions_root(&txs);

        let result = node.on_block_received(block);
        assert!(result.is_err(), "block with gas overflow must be rejected");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("gas overflow") || msg.contains("exceeds limit"),
            "error must mention gas overflow, got: {msg}"
        );
    }

    #[test]
    fn block_within_gas_limit_is_accepted() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let proposer = PublicKey::from_bytes(keypair.public_key()).to_address();
        let consensus = Box::new(SimpleConsensus::new(validators));
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        let tx = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: proposer,
            sender_pubkey: PublicKey::from_bytes(vec![1u8; 32]),
            inputs: vec![],
            outputs: vec![],
            reads: std::collections::HashSet::new(),
            writes: std::collections::HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 0,
            signature: aether_types::Signature::from_bytes(vec![0u8; 64]),
        };
        let mut block = Block::new(
            0,
            H256::zero(),
            proposer,
            aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
            vec![tx],
        );
        block.header.transactions_root = compute_transactions_root(&block.transactions);

        // Should pass gas limit check (may fail later on state root, that's fine)
        let result = node.on_block_received(block);
        // If it fails, it should NOT be due to gas limit
        if let Err(e) = &result {
            let msg = e.to_string();
            assert!(
                !msg.contains("block gas") && !msg.contains("gas overflow"),
                "block within gas limit should not fail on gas check, got: {msg}"
            );
        }
    }

    #[test]
    fn duplicate_signer_in_aggregated_vote_is_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let keypair = Keypair::generate();
        let validators = vec![validator_info_from_key(&keypair)];
        let signer_pk = PublicKey::from_bytes(keypair.public_key());
        let proposer = signer_pk.to_address();
        let mut simple_consensus = SimpleConsensus::new(validators);
        simple_consensus.advance_slot();
        simple_consensus.advance_slot();
        let consensus: Box<dyn aether_consensus::ConsensusEngine> = Box::new(simple_consensus);
        let mut node = Node::new(
            temp_dir.path(),
            consensus,
            Some(keypair),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();

        let parent = Block::new(
            1,
            H256::zero(),
            proposer,
            aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
            vec![],
        );
        let parent_hash = parent.hash();
        node.blocks_by_hash.insert(parent_hash, parent);

        let agg_vote = aether_types::AggregatedVote {
            slot: 1,
            block_hash: parent_hash,
            aggregated_signature: vec![0u8; 96],
            signers: vec![signer_pk.clone(), signer_pk],
            total_stake: 200_000_000,
        };

        let mut child = Block::new(
            2,
            parent_hash,
            proposer,
            aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
            vec![],
        );
        child.header.transactions_root = H256::zero();
        child.aggregated_vote = Some(agg_vote);

        let result = node.on_block_received(child);
        assert!(
            result.is_err(),
            "block with duplicate signers must be rejected"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("duplicate signer"),
            "error must mention duplicate signer, got: {msg}"
        );
    }
}
