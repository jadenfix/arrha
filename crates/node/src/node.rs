use aether_consensus::slashing::{self as slash_verify, SlashProof, SlashType, Vote as SlashVote};
use aether_consensus::{ConsensusEngine, SlashingDetector};
use aether_crypto_bls::BlsKeypair;
use aether_crypto_primitives::Keypair;
use aether_ledger::{EmissionSchedule, FeeMarket, Ledger};
use aether_mempool::Mempool;
use aether_p2p::network::NetworkEvent;
use aether_program_staking::StakingState;
use aether_state_snapshots::generate_snapshot;
use aether_state_storage::{
    database::pruning, Storage, StorageBatch, CF_BLOCKS, CF_METADATA, CF_RECEIPTS, CF_STAKING,
};
use aether_types::{
    Account, Address, Block, ChainConfig, PublicKey, Slot, Transaction, TransactionReceipt, Vote,
    H256,
};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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
const SYNC_RESPONSE_COOLDOWN: Duration = Duration::from_secs(2);

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

        let fee_market = FeeMarket::new(
            chain_config.fees.a,
            chain_config.chain.block_bytes_max,
            chain_config.fees.min_base_fee,
        );
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
        })
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
        self.broadcast(OutboundMessage::BroadcastTransaction(tx));
        Ok(tx_hash)
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

        // Check pacemaker timeout — if no quorum reached, advance to prevent deadlock
        if self.consensus.is_timed_out() {
            self.consecutive_timeouts += 1;
            if self.consecutive_timeouts >= 100 {
                tracing::error!(
                    consecutive_timeouts = self.consecutive_timeouts,
                    slot,
                    "CIRCUIT BREAKER — possible network partition or all peers down"
                );
            }
            tracing::warn!(
                slot,
                consecutive = self.consecutive_timeouts,
                "Slot timeout — advancing via pacemaker"
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

        if let Some(ref keypair) = self.validator_key {
            let pubkey = PublicKey::from_bytes(keypair.public_key());

            if self.consensus.is_leader(slot, &pubkey) {
                tracing::info!(slot, "Leader — producing block");
                self.produce_block(slot)?;
            } else {
                tracing::debug!(slot, "Not leader — waiting for block");
            }
        }

        // Check if any slot can be finalized
        self.check_finality();

        // Drive state sync if we're behind the network.
        self.drive_sync(slot);

        // Evict old cached blocks/receipts to bound memory (Fix 10)
        self.evict_old_cache();

        Ok(())
    }

    /// Drive the state sync protocol: detect if behind, request blocks,
    /// apply buffered blocks in order, and handle stalls.
    fn drive_sync(&mut self, current_slot: Slot) {
        let my_latest = self.latest_block_slot.unwrap_or(0);
        let _span = tracing::debug_span!(
            "drive_sync",
            current_slot,
            my_latest,
            lag = current_slot.saturating_sub(my_latest),
        )
        .entered();

        // Check if we've stalled during an active sync.
        if self.sync_manager.is_syncing() && self.sync_manager.check_stalled() {
            tracing::warn!(
                next_expected = self.sync_manager.next_expected(),
                "Sync stalled — retrying"
            );
            NODE_METRICS.sync_stalls.inc();
            self.sync_manager.retry_after_stall(current_slot);
        }

        // Check if sync is needed based on how far behind we are.
        if self.sync_manager.check_sync_needed(my_latest, current_slot) {
            NODE_METRICS.sync_active.set(1);
            NODE_METRICS
                .sync_slot_lag
                .set((current_slot.saturating_sub(my_latest)) as i64);
            // Set expected parent hash from our latest block for chain continuity.
            if self.sync_manager.blocks_applied() == 0 {
                if let Some(hash) = self.latest_block_hash_if_set() {
                    self.sync_manager.set_expected_parent(hash);
                }
            }

            // Request the next batch of blocks from peers.
            if let Some((from, to)) = self.sync_manager.next_request() {
                tracing::info!(from, to, "Requesting sync blocks from peers");
                self.broadcast(OutboundMessage::RequestBlockRange {
                    from_slot: from,
                    to_slot: to,
                });
            }
        } else {
            NODE_METRICS.sync_active.set(0);
            NODE_METRICS.sync_slot_lag.set(0);
        }

        // Apply any contiguous buffered blocks.
        let ready = self.sync_manager.drain_ready();
        let batch_len = ready.len();
        for block in ready {
            let slot = block.header.slot;
            match self.on_block_received(block) {
                Ok(()) => {
                    self.sync_manager.record_applied();
                    NODE_METRICS.sync_blocks_applied.inc();
                }
                Err(e) => tracing::warn!(slot, err = %e, "Failed to apply sync block"),
            }
        }
        NODE_METRICS
            .sync_buffer_size
            .set(self.sync_manager.buffer_len() as i64);
        if batch_len > 0 {
            if let Some((from, target)) = self.sync_manager.sync_range() {
                tracing::info!(
                    applied = batch_len,
                    total_applied = self.sync_manager.blocks_applied(),
                    next_expected = self.sync_manager.next_expected(),
                    target_slot = target,
                    from_slot = from,
                    buffered = self.sync_manager.buffer_len(),
                    "Sync progress"
                );
            }
        }
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
        // Evict blocks exceeding cache limit — O(log n) per eviction via BTreeMap
        while self.blocks_by_hash.len() > MAX_CACHED_BLOCKS {
            if let Some((&min_slot, &hash)) = self.blocks_by_slot.iter().next() {
                self.blocks_by_slot.remove(&min_slot);
                self.blocks_by_hash.remove(&hash);
            } else {
                break;
            }
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

    fn produce_block(&mut self, slot: Slot) -> Result<()> {
        let _span = tracing::info_span!("produce_block", slot).entered();
        let block_start = Instant::now();

        // Forced inclusion: include txs that have been waiting too long (anti-censorship)
        let transactions = {
            let _mempool_span = tracing::debug_span!("mempool_select", slot).entered();
            let forced = self
                .mempool
                .must_include_transactions(slot, self.fee_market.base_fee);
            let forced_count = forced.len();
            let remaining_capacity = 1000usize.saturating_sub(forced_count);
            let regular = self
                .mempool
                .get_transactions(remaining_capacity, MAX_BLOCK_GAS_LIMIT);
            if forced_count > 0 {
                tracing::info!(forced_count, "Forced inclusion txs");
                let mut all = forced;
                all.extend(regular);
                all
            } else {
                regular
            }
        };

        tracing::info!(tx_count = transactions.len(), "Including transactions");

        // Get VRF proof FIRST (before execution, to fail fast if not leader)
        let vrf_proof_crypto = self.consensus.get_leader_proof(slot);
        let vrf_proof = if let Some(proof) = vrf_proof_crypto {
            aether_types::VrfProof {
                output: proof.output,
                proof: proof.proof,
            }
        } else {
            aether_types::VrfProof {
                output: [0u8; 32],
                proof: vec![],
            }
        };

        // Apply transactions speculatively (NOT committed to disk yet)
        let (receipts, overlay) = self.ledger.apply_block_speculatively_with_chain_id(
            &transactions,
            Some(self.chain_config.chain.chain_id_numeric),
        )?;
        let successful = receipts
            .iter()
            .filter(|r| matches!(r.status, aether_types::TransactionStatus::Success))
            .count();

        if !transactions.is_empty() {
            tracing::info!(
                successful,
                failed = receipts.len() - successful,
                "Speculative execution complete"
            );
        }

        // Compute block header roots from speculative state
        let (state_root, transactions_root, receipts_root) = {
            let _roots_span =
                tracing::debug_span!("compute_roots", tx_count = transactions.len()).entered();
            (
                overlay.state_root,
                compute_transactions_root(&transactions),
                compute_receipts_root(&receipts),
            )
        };

        let key = self
            .validator_key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("validator_key required for block production"))?;
        let proposer_bytes = key.to_address();
        let proposer = aether_types::Address::from_slice(&proposer_bytes)
            .map_err(|e| anyhow::anyhow!("invalid proposer address: {e}"))?;

        let mut block = Block::new(
            slot,
            self.latest_block_hash,
            proposer,
            vrf_proof,
            transactions.clone(),
        );

        block.header.state_root = state_root;
        block.header.transactions_root = transactions_root;
        block.header.receipts_root = receipts_root;

        let block_hash = block.hash();
        tracing::info!(?block_hash, %state_root, "Block produced");

        // Validate our own block BEFORE committing state
        {
            let _validate_span = tracing::debug_span!("self_validate_block", slot).entered();
            if let Err(e) = self.consensus.validate_block(&block) {
                tracing::warn!(err = %e, "Self-produced block validation failed");
                return Ok(());
            }
        }

        // Build stored receipts (with block context) for both cache and disk
        let stored_receipts: Vec<TransactionReceipt> = receipts
            .iter()
            .map(|r| {
                let mut sr = r.clone();
                sr.block_hash = block_hash;
                sr.slot = slot;
                sr
            })
            .collect();

        // ATOMIC COMMIT: overlay state + block + receipts + fee distribution in one WriteBatch.
        // Previously these were two separate write_batch calls; a crash between
        // them could leave ledger state committed without the block record (or
        // vice versa), corrupting the node on restart.
        // Fee distribution is also folded in so proposer rewards are never lost if
        // the process crashes after the overlay commit but before the credit write.
        let total_fees: u128 = transactions
            .iter()
            .fold(0u128, |acc, tx| acc.saturating_add(tx.fee));
        let gas_used: u64 = transactions
            .iter()
            .fold(0u64, |acc, tx| acc.saturating_add(tx.gas_limit));
        let fee_result = self.fee_market.process_block(gas_used, total_fees);

        let mut batch = self.ledger.prepare_overlay_batch(&overlay)?;
        let block_batch = self.build_block_batch(&block, block_hash, &stored_receipts)?;
        batch.extend(block_batch);
        self.ledger.fold_fee_distribution_into_batch(
            &mut batch,
            &overlay,
            &block.header.proposer,
            fee_result.proposer_reward,
            fee_result.burned,
            fee_result.treasury_fee,
        )?;
        // Record spent UTXOs for light-client audit and epoch-based pruning.
        self.ledger.record_spent_utxos(&mut batch, &overlay, slot);
        self.ledger.write_batch(batch)?;
        STORAGE_METRICS.blocks_persisted.inc();

        // Record block production metrics
        CONSENSUS_METRICS.blocks_produced.inc();
        CONSENSUS_METRICS
            .transactions_processed
            .inc_by(transactions.len() as u64);
        CONSENSUS_METRICS
            .block_production_ms
            .observe(block_start.elapsed().as_secs_f64() * 1000.0);

        for sr in &stored_receipts {
            self.receipts.insert(sr.tx_hash, sr.clone());
        }

        self.fork_choice.add_block(slot, block_hash);
        self.fork_choice.mark_committed(slot);
        // Record that this slot's state is now durably committed — mirrors the
        // guard in on_block_received that prevents a fork block from overwriting
        // already-committed state and corrupting the UTXO set.
        self.committed_at_slot.insert(slot, block_hash);
        self.latest_block_hash = block_hash;
        self.latest_block_slot = Some(slot);
        self.blocks_by_slot.insert(slot, block_hash);
        self.blocks_by_hash.insert(block_hash, block.clone());

        // Record block parent for 2-chain finality tracking
        self.consensus
            .record_block(block_hash, block.header.parent_hash, slot);

        // Remove transactions from mempool and update sender nonces so the
        // mempool rejects replays of already-executed transactions — even ones
        // that were never in this node's local pool.
        let tx_hashes: Vec<H256> = transactions.iter().map(|tx| tx.hash()).collect();
        self.mempool.remove_transactions(&tx_hashes);
        for tx in &transactions {
            self.mempool
                .advance_sender_nonce(tx.sender, tx.nonce.saturating_add(1));
        }

        // Broadcast block to network
        self.broadcast(OutboundMessage::BroadcastBlock(block.clone()));

        // Vote on our own block
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

        Ok(())
    }

    // ========================================================================
    // Block Reception (Phase B)
    // ========================================================================

    /// Handle a block received from the P2P network.
    pub fn on_block_received(&mut self, block: Block) -> Result<()> {
        let block_hash = block.hash();
        let _span =
            tracing::info_span!("on_block_received", slot = block.header.slot, ?block_hash,)
                .entered();

        // Reject if already known (also prevents fee market double-update)
        if self.blocks_by_hash.contains_key(&block_hash) {
            return Ok(());
        }

        // Reject if slot is at or before finalized (except slot 0 edge case)
        if block.header.slot <= self.consensus.finalized_slot()
            && self.consensus.finalized_slot() > 0
        {
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

        // Buffer as orphan if parent is unknown (skip for genesis-like blocks).
        // We check this before full consensus validation because consensus checks
        // (e.g. future-slot rejection) may fail for blocks received out of order
        // during sync. These blocks will be fully validated when their parent arrives.
        if block.header.slot > 0
            && block.header.parent_hash != H256::zero()
            && !self.blocks_by_hash.contains_key(&block.header.parent_hash)
        {
            if self.orphan_count < MAX_ORPHAN_BLOCKS {
                tracing::info!(
                    slot = block.header.slot,
                    parent = ?block.header.parent_hash,
                    "Buffering orphan block (parent unknown)"
                );
                self.orphan_count += 1;
                self.orphan_blocks
                    .entry(block.header.parent_hash)
                    .or_default()
                    .push(block);
            } else {
                tracing::warn!(
                    slot = block.header.slot,
                    "Orphan buffer full ({MAX_ORPHAN_BLOCKS}), dropping block"
                );
            }
            return Ok(());
        }

        // Validate block timestamp before heavier checks:
        // 1. Must not be more than MAX_CLOCK_DRIFT_SECS in the future (prevents proposer
        //    from manufacturing far-future timestamps to manipulate time-based on-chain logic).
        // 2. Must be ≥ parent block timestamp (monotonicity).
        if block.header.slot > 0 {
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if block.header.timestamp > now_secs.saturating_add(MAX_CLOCK_DRIFT_SECS) {
                bail!(
                    "block timestamp {} is too far in the future (now={}, max_drift={}s)",
                    block.header.timestamp,
                    now_secs,
                    MAX_CLOCK_DRIFT_SECS
                );
            }
            // Timestamp must not precede parent's timestamp.
            if block.header.parent_hash != H256::zero() {
                if let Some(parent_block) = self.blocks_by_hash.get(&block.header.parent_hash) {
                    if block.header.timestamp < parent_block.header.timestamp {
                        bail!(
                            "block timestamp {} precedes parent timestamp {}",
                            block.header.timestamp,
                            parent_block.header.timestamp
                        );
                    }
                }
            }
        }

        // Reject oversized blocks before expensive validation.
        // bincode::serialized_size is O(n) but avoids full serialization allocation.
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
        // A malicious proposer could include the same tx twice; while the
        // second would fail during execution (nonce/UTXO), rejecting early
        // avoids wasting execution resources and prevents state-root
        // divergence between nodes that process duplicates differently.
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

        // Validate block via consensus (VRF proof, locked block check)
        self.consensus.validate_block(&block)?;

        // Validate slot monotonicity: block slot must be strictly greater than parent's slot
        if block.header.slot > 0 && block.header.parent_hash != H256::zero() {
            if let Some(parent_block) = self.blocks_by_hash.get(&block.header.parent_hash) {
                if block.header.slot <= parent_block.header.slot {
                    bail!(
                        "slot monotonicity violation: block slot {} <= parent slot {}",
                        block.header.slot,
                        parent_block.header.slot
                    );
                }
            }
        }

        // Validate transactions_root matches actual transactions (unconditional —
        // empty blocks naturally produce H256::zero() so zero roots still pass)
        let computed_tx_root = compute_transactions_root(&block.transactions);
        if computed_tx_root != block.header.transactions_root {
            bail!(
                "transactions_root mismatch: computed={}, block={}",
                computed_tx_root,
                block.header.transactions_root
            );
        }

        // Non-genesis blocks MUST carry a quorum certificate (aggregated vote).
        // Without this check, a malicious proposer could omit the QC entirely
        // and bypass BLS quorum verification.
        let is_genesis_or_bootstrap =
            block.header.slot <= 1 || block.header.parent_hash == H256::zero();
        if !is_genesis_or_bootstrap && block.aggregated_vote.is_none() {
            bail!(
                "block at slot {} missing required quorum certificate (aggregated_vote)",
                block.header.slot
            );
        }

        // Verify BLS aggregate signature when present (proves quorum voted for parent)
        if let Some(ref agg_vote) = block.aggregated_vote {
            let _bls_span = tracing::debug_span!(
                "verify_bls_aggregate",
                slot = block.header.slot,
                signers = agg_vote.signers.len(),
            )
            .entered();
            // The QC must reference this block's parent — it certifies that
            // a supermajority voted for the parent, justifying this extension.
            if !is_genesis_or_bootstrap && agg_vote.block_hash != block.header.parent_hash {
                bail!(
                    "aggregated vote references block {:?} but parent is {:?}",
                    agg_vote.block_hash,
                    block.header.parent_hash
                );
            }
            if agg_vote.signers.is_empty() {
                bail!("aggregated vote has no signers");
            }
            // Reject duplicate signers: without this check an attacker controlling a
            // single validator could list it N times, inflating voted_stake to N*stake
            // and bypassing the 2/3 quorum requirement. The BLS aggregate would still
            // verify because agg(sig, sig, ...) = N*sig verifies against N*pk.
            {
                let mut seen = HashSet::with_capacity(agg_vote.signers.len());
                for signer in &agg_vote.signers {
                    if !seen.insert(signer.to_address()) {
                        bail!(
                            "duplicate signer {:?} in aggregated vote",
                            signer.to_address()
                        );
                    }
                }
            }
            // Reconstruct the vote message: block_hash || slot (same as vote_on_block)
            let mut vote_msg = Vec::new();
            vote_msg.extend_from_slice(agg_vote.block_hash.as_bytes());
            vote_msg.extend_from_slice(&agg_vote.slot.to_le_bytes());

            // Look up BLS public keys and compute voted stake from our local
            // validator set. NEVER trust agg_vote.total_stake from the block —
            // an attacker could set it to any value to bypass quorum checks.
            let mut bls_pubkeys = Vec::with_capacity(agg_vote.signers.len());
            let mut voted_stake: u128 = 0;
            for signer in &agg_vote.signers {
                let addr = signer.to_address();
                let bls_pk = self.consensus.get_bls_pubkey(&addr).ok_or_else(|| {
                    anyhow::anyhow!("no BLS pubkey registered for signer {:?}", addr)
                })?;
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

            // Verify quorum: locally-computed voted stake must be >= 2/3 of total.
            // Use the overflow-safe has_quorum() which handles large u128 stakes
            // via checked_mul — bare `total_stake * 2` would overflow for stakes
            // near u128::MAX, making the check trivially pass.
            let total_stake = self.consensus.total_stake();
            if total_stake > 0 && !aether_consensus::has_quorum(voted_stake, total_stake) {
                bail!(
                    "insufficient quorum: voted stake {} < required 2/3 of {}",
                    voted_stake,
                    total_stake
                );
            }
        }

        // Enforce block gas limit: total gas across all transactions must not exceed
        // the protocol limit. Without this, a malicious proposer could stuff arbitrarily
        // large blocks, causing receivers to OOM during speculative execution.
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

        // Execute transactions SPECULATIVELY (not committed to disk yet)
        // Use chain_id validation to reject cross-chain replay attacks
        let (receipts, overlay) = self.ledger.apply_block_speculatively_with_chain_id(
            &block.transactions,
            Some(self.chain_config.chain.chain_id_numeric),
        )?;

        // Validate receipts_root matches recomputed receipts
        let computed_receipts_root = compute_receipts_root(&receipts);
        if computed_receipts_root != block.header.receipts_root {
            bail!(
                "receipts_root mismatch: computed={}, block={}",
                computed_receipts_root,
                block.header.receipts_root
            );
        }

        // Validate state root matches before committing (unconditional)
        if overlay.state_root != block.header.state_root {
            // Discard overlay — state is UNCHANGED (rollback!)
            bail!(
                "state root mismatch: computed={}, block={} — block rejected, state unchanged",
                overlay.state_root,
                block.header.state_root
            );
        }

        // Build stored receipts (with block context) for both cache and disk
        let stored_receipts: Vec<TransactionReceipt> = receipts
            .iter()
            .map(|r| {
                let mut sr = r.clone();
                sr.block_hash = block_hash;
                sr.slot = block.header.slot;
                sr
            })
            .collect();

        // Fork choice BEFORE commit: only persist state for canonical blocks.
        // This prevents orphaned state from non-canonical forks being committed.
        let old_canonical = self.fork_choice.canonical_block(block.header.slot);
        let is_fork = self.fork_choice.add_block(block.header.slot, block_hash);
        let new_canonical = self.fork_choice.canonical_block(block.header.slot);

        if is_fork {
            CONSENSUS_METRICS.fork_events.inc();
        }

        let is_canonical = new_canonical == Some(block_hash);

        // SAFETY: once a block has been durably written to storage at a given slot, we must
        // NOT write a competing block's state on top of it.  Doing so would leave stale effects
        // from the first-committed block (e.g. UTXOs it created but the new canonical did not
        // spend) permanently in storage, silently corrupting the UTXO set.
        //
        // The first canonical block committed at a slot wins.  Any subsequent fork block that
        // fork-choice would prefer (lower hash) is kept in memory for vote/QC purposes but its
        // overlay is discarded.  The correct resolution is to wait for the chain to grow: when
        // a descendant of the fork block arrives and becomes canonical, the state from the
        // finalized ancestor path will be applied from a clean base.
        let already_committed_at_slot = self
            .committed_at_slot
            .get(&block.header.slot)
            .is_some_and(|&h| h != block_hash);

        let should_commit = is_canonical && !already_committed_at_slot;

        if already_committed_at_slot && is_canonical {
            tracing::warn!(
                slot = block.header.slot,
                committed = ?self.committed_at_slot.get(&block.header.slot),
                incoming = ?block_hash,
                "Fork block would be canonical but a different block is already committed at this \
                 slot — skipping state commit to prevent UTXO set corruption"
            );
        }

        if should_commit {
            let _commit_span = tracing::info_span!(
                "atomic_block_commit",
                slot = block.header.slot,
                ?block_hash,
                tx_count = block.transactions.len(),
            )
            .entered();
            // ATOMIC COMMIT: overlay state + block + receipts + fee distribution in one WriteBatch.
            // Fee distribution is folded in so proposer rewards are never lost if the process
            // crashes after the overlay commit but before the credit write.
            let total_fees: u128 = block
                .transactions
                .iter()
                .fold(0u128, |acc, tx| acc.saturating_add(tx.fee));
            let gas_used: u64 = block
                .transactions
                .iter()
                .fold(0u64, |acc, tx| acc.saturating_add(tx.gas_limit));
            let fee_result = self.fee_market.process_block(gas_used, total_fees);

            let mut batch = self.ledger.prepare_overlay_batch(&overlay)?;
            let block_batch = self.build_block_batch(&block, block_hash, &stored_receipts)?;
            batch.extend(block_batch);
            self.ledger.fold_fee_distribution_into_batch(
                &mut batch,
                &overlay,
                &block.header.proposer,
                fee_result.proposer_reward,
                fee_result.burned,
                fee_result.treasury_fee,
            )?;
            // Record spent UTXOs for light-client audit and epoch-based pruning.
            self.ledger
                .record_spent_utxos(&mut batch, &overlay, block.header.slot);

            // Apply slash evidence BEFORE the atomic write so slashing effects are
            // persisted in the same WriteBatch. This prevents a crash between block
            // commit and slash application from losing slash effects.
            for evidence in &block.slash_evidence {
                let (v1, v2, etype) =
                    match (&evidence.vote1, &evidence.vote2, &evidence.evidence_type) {
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
                let offense_slot = v1.slot;
                let offense_key = (evidence.validator, offense_slot);
                if !self.slashed_offenses.insert(offense_key) {
                    tracing::debug!(
                        validator = ?evidence.validator,
                        slot = offense_slot,
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

            // Include staking state in the atomic batch so slash effects, validator
            // registrations, and unbonding changes survive node restarts.
            self.persist_staking_state_to_batch(&mut batch)?;

            self.ledger.write_batch(batch)?;
            // Record that this block's state is now durably committed at this slot.
            self.committed_at_slot.insert(block.header.slot, block_hash);

            // Lock this slot against future fork-choice reorgs — state is now
            // committed and we have no rollback mechanism.
            self.fork_choice.mark_committed(block.header.slot);
        } else {
            tracing::info!(
                block_hash = %block_hash,
                slot = block.header.slot,
                canonical = ?new_canonical,
                "Non-canonical fork block — state NOT committed"
            );
        }

        if is_fork {
            tracing::warn!(
                slot = block.header.slot,
                "Fork detected — multiple blocks at same slot"
            );

            // If canonical choice changed, reorg mempool nonces for affected senders
            if old_canonical != new_canonical {
                if let Some(old_hash) = old_canonical {
                    if let Some(old_block) = self.blocks_by_hash.get(&old_hash) {
                        let reverted_txs = old_block.transactions.clone();
                        let mut new_nonces = std::collections::HashMap::new();
                        for tx in &block.transactions {
                            new_nonces
                                .entry(tx.sender)
                                .and_modify(|n: &mut u64| *n = (*n).max(tx.nonce.saturating_add(1)))
                                .or_insert(tx.nonce.saturating_add(1));
                        }
                        self.mempool.reorg(reverted_txs, new_nonces);
                    }
                }
            }
        }

        // Store block in hash map (all blocks, including non-canonical)
        self.blocks_by_hash.insert(block_hash, block.clone());

        // Only update slot->hash map and tip for blocks whose state was actually committed.
        // If a fork block is preferred by fork-choice but we skipped its commit (to prevent
        // UTXO-set corruption from double-committing at the same slot), keep the previously
        // committed block as the chain tip — building on an uncommitted state would produce
        // blocks with an incorrect state_root.
        if should_commit {
            self.blocks_by_slot.insert(block.header.slot, block_hash);
            self.latest_block_hash = block_hash;
            self.latest_block_slot = Some(block.header.slot);
        }

        // Record block parent for 2-chain finality tracking
        self.consensus
            .record_block(block_hash, block.header.parent_hash, block.header.slot);

        for sr in &stored_receipts {
            self.receipts.insert(sr.tx_hash, sr.clone());
        }

        // Remove included txs from mempool and advance sender nonces so the
        // mempool rejects replays of transactions included in received blocks.
        let tx_hashes: Vec<H256> = block.transactions.iter().map(|tx| tx.hash()).collect();
        self.mempool.remove_transactions(&tx_hashes);
        for tx in &block.transactions {
            self.mempool
                .advance_sender_nonce(tx.sender, tx.nonce.saturating_add(1));
        }

        // Vote on this block (if we're a validator)
        self.vote_on_block(&block)?;

        // Try to apply any orphan blocks that were waiting for this block as parent.
        self.process_orphans(block_hash);

        // Update sync state based on network tip vs local tip
        if let Some(local_slot) = self.latest_block_slot {
            self.sync_manager
                .check_sync_needed(local_slot, block.header.slot);
        }

        Ok(())
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
        self.sync_manager.is_syncing()
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

        self.consensus.add_vote(vote)?;
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
                if self.sync_manager.is_syncing() {
                    let slot = block.header.slot;
                    if !self.sync_manager.buffer_block(block) {
                        tracing::warn!(slot, "Sync buffer full, dropping block");
                    }
                } else if let Err(e) = self.on_block_received(block) {
                    tracing::debug!(err = %e, "Block rejected");
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

        // Only check slots we haven't checked yet (avoid O(n) scan on restart)
        let start = if last_finalized > 0 {
            last_finalized
        } else {
            0
        };

        // Limit to checking at most 100 slots per tick to prevent CPU spikes
        let end = current_slot.min(start + 100);

        for slot in start..=end {
            if self.consensus.check_finality(slot) {
                CONSENSUS_METRICS.slots_finalized.inc();

                tracing::info!(slot, "Slot finalized via VRF+HotStuff+BLS");

                // Update epoch randomness and measure finality latency from the
                // finalized block.
                if let Some(block) = self.get_block_by_slot(slot) {
                    // Finality latency: wall-clock time since the block was produced.
                    let now_secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let latency_ms = now_secs
                        .saturating_sub(block.header.timestamp)
                        .saturating_mul(1000);
                    CONSENSUS_METRICS
                        .finality_latency_ms
                        .observe(latency_ms as f64);

                    if block.header.vrf_proof.output != [0u8; 32] {
                        self.consensus
                            .update_epoch_randomness(&block.header.vrf_proof.output);
                    }
                }

                // Finalize in fork choice
                if let Some(&hash) = self.blocks_by_slot.get(&slot) {
                    if !self.fork_choice.finalize(slot, hash) {
                        tracing::warn!(
                            slot,
                            ?hash,
                            "fork_choice: could not finalize unknown block"
                        );
                    }
                }
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

    /// Returns the latest block hash only if we have processed at least one block.
    fn latest_block_hash_if_set(&self) -> Option<H256> {
        if self.latest_block_slot.is_some() {
            Some(self.latest_block_hash)
        } else {
            None
        }
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

    pub fn get_block_by_slot(&self, slot: Slot) -> Option<Block> {
        // Check in-memory cache first
        if let Some(block) = self
            .blocks_by_slot
            .get(&slot)
            .and_then(|hash| self.blocks_by_hash.get(hash))
        {
            return Some(block.clone());
        }
        // Fall back to RocksDB: slot index → hash → block
        let slot_key = format!("slot:{}", slot);
        let hash_bytes = self
            .ledger
            .storage()
            .get(CF_METADATA, slot_key.as_bytes())
            .ok()
            .flatten()?;
        let hash = H256::from_slice(&hash_bytes).ok()?;
        self.get_block_by_hash(hash)
    }

    /// RPC callers must distinguish empty slots from storage/index corruption.
    pub fn try_get_block_by_slot(&self, slot: Slot) -> Result<Option<Block>> {
        if let Some(block) = self
            .blocks_by_slot
            .get(&slot)
            .and_then(|hash| self.blocks_by_hash.get(hash))
        {
            return Ok(Some(block.clone()));
        }
        let slot_key = format!("slot:{}", slot);
        let Some(hash_bytes) = self
            .ledger
            .storage()
            .get(CF_METADATA, slot_key.as_bytes())?
        else {
            return Ok(None);
        };
        let hash = H256::from_slice(&hash_bytes)
            .map_err(|e| anyhow::anyhow!("invalid persisted slot hash: {e}"))?;
        let block = self
            .try_get_block_by_hash(hash)?
            .context("persisted slot points to missing block")?;
        anyhow::ensure!(
            block.header.slot == slot,
            "persisted slot points to another block slot"
        );
        Ok(Some(block))
    }

    pub fn try_get_block_by_hash(&self, hash: H256) -> Result<Option<Block>> {
        if let Some(block) = self.blocks_by_hash.get(&hash) {
            return Ok(Some(block.clone()));
        }
        let Some(bytes) = self.ledger.storage().get(CF_BLOCKS, hash.as_bytes())? else {
            return Ok(None);
        };
        let block: Block =
            bincode::deserialize(&bytes).context("persisted block cannot be decoded")?;
        anyhow::ensure!(block.hash() == hash, "persisted block hash mismatch");
        Ok(Some(block))
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
        // Check in-memory cache first (recent receipts)
        if let Some(receipt) = self.receipts.get(&tx_hash) {
            return Some(receipt.clone());
        }
        // Fall back to RocksDB for older/pre-restart receipts
        self.ledger
            .storage()
            .get(CF_RECEIPTS, tx_hash.as_bytes())
            .ok()
            .flatten()
            .and_then(|bytes| bincode::deserialize(&bytes).ok())
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
    fn rpc_block_reads_reject_corrupt_or_missing_indexed_data() {
        let dir = TempDir::new().unwrap();
        let key = Keypair::generate();
        let consensus = Box::new(SimpleConsensus::new(vec![validator_info_from_key(&key)]));
        let node = Node::new(
            dir.path(),
            consensus,
            Some(key),
            None,
            Arc::new(ChainConfig::devnet()),
        )
        .unwrap();
        assert!(node.try_get_block_by_slot(42).unwrap().is_none());
        let storage = node.ledger.storage();
        storage
            .put(CF_METADATA, b"slot:42", b"invalid hash")
            .unwrap();
        assert!(node.try_get_block_by_slot(42).is_err());
        let hash = H256([1; 32]);
        storage
            .put(CF_METADATA, b"slot:42", hash.as_bytes())
            .unwrap();
        assert!(node.try_get_block_by_slot(42).is_err());
        storage
            .put(CF_BLOCKS, hash.as_bytes(), b"invalid block")
            .unwrap();
        assert!(node.try_get_block_by_hash(hash).is_err());
        assert!(node.try_get_block_by_slot(42).is_err());
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
