use aether_consensus::slashing::{self as slash_verify, SlashProof, SlashType, Vote as SlashVote};
use aether_consensus::{ConsensusEngine, SlashingDetector};
use aether_crypto_bls::BlsKeypair;
use aether_crypto_primitives::Keypair;
use aether_ledger::{
    BlockContext, EmissionSchedule, FeeMarket, Ledger, PendingOverlay, SparseMerkleTree,
};
use aether_mempool::Mempool;
use aether_p2p::network::NetworkEvent;
use aether_program_staking::{StakingState, Validator as StakingValidator};
use aether_state_snapshots::generate_snapshot;
use aether_state_storage::{
    database::pruning, Storage, StorageBatch, CF_BLOCKS, CF_METADATA, CF_RECEIPTS, CF_STAKING,
};
use aether_types::saturating_mul_div as mul_div;
use aether_types::{
    Account, Address, AggregatedVote, Block, ChainConfig, PublicKey, SlashEvidence, Slot,
    Transaction, TransactionReceipt, TransactionStatus, Vote, H256,
};
use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time;

use aether_metrics::{CONSENSUS_METRICS, NODE_METRICS, STORAGE_METRICS};

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
const META_CHAIN_IDENTITY: &[u8] = b"chain_identity";
const META_PROTOCOL_VERSION: &[u8] = b"protocol_version";
const META_SLASHED_OFFENSES: &[u8] = b"slashed_offenses";
const META_PENDING_SLASH_EVIDENCE: &[u8] = b"pending_slash_evidence";
const MAX_PENDING_SLASH_EVIDENCE: usize = 128;

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
#[derive(Clone)]
struct PendingBlock {
    block: Block,
    overlay: Arc<PendingOverlay>,
    /// Merkle tree after this block (including fee distribution).
    tree: SparseMerkleTree,
    /// Receipts with block hash and slot filled in.
    receipts: Vec<TransactionReceipt>,
    /// Fee market state after this block.
    fee_market: FeeMarket,
    /// Branch-local staking state, including any unbondings completed by this
    /// block. It is adopted only when this block finalizes.
    staking_state: StakingState,
    slash_events: Vec<(Address, Slot, u128)>,
    slashed_offenses: HashSet<(Address, Slot)>,
}

/// Output of executing a block on top of its parent.
struct ExecutedBlock {
    receipts: Vec<TransactionReceipt>,
    overlay: PendingOverlay,
    tree: SparseMerkleTree,
    fee_market: FeeMarket,
    staking_state: StakingState,
    slash_events: Vec<(Address, Slot, u128)>,
    slashed_offenses: HashSet<(Address, Slot)>,
    /// State root after all block effects, including fee distribution.
    /// This is the state committed by the header and adopted on finalization.
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
    /// Ephemeral duplicate suppression for diagnostic vote-time stake changes.
    detected_offenses: HashSet<(Address, u64)>,
    /// Verified proofs waiting for block inclusion. Retained across losing
    /// branches and removed only after a canonical block commits the offense.
    pending_slash_evidence: HashMap<(Address, Slot), SlashEvidence>,
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
    /// Bind this database to the exact chain configuration and genesis identity.
    /// Legacy databases without an identity are bound once; subsequent starts
    /// must present the same identity.
    pub fn bind_chain_identity(&mut self, identity: H256) -> Result<()> {
        match self
            .ledger
            .storage()
            .get(CF_METADATA, META_CHAIN_IDENTITY)?
        {
            Some(bytes) if bytes.as_slice() == identity.as_bytes() => Ok(()),
            Some(_) => bail!("database chain identity does not match configured chain"),
            None => {
                self.ledger
                    .storage()
                    .put(CF_METADATA, META_CHAIN_IDENTITY, identity.as_bytes())
            }
        }
    }

    pub fn new<P: AsRef<Path>>(
        db_path: P,
        mut consensus: Box<dyn ConsensusEngine>,
        validator_key: Option<Keypair>,
        bls_key: Option<BlsKeypair>,
        chain_config: Arc<ChainConfig>,
    ) -> Result<Self> {
        let storage = Storage::open(db_path).context("failed to open storage")?;
        match storage.get(CF_METADATA, META_PROTOCOL_VERSION)? {
            Some(bytes) if bytes.len() == 4 => {
                let stored = u32::from_le_bytes(bytes.try_into().expect("checked length"));
                if stored != aether_types::PROTOCOL_VERSION {
                    bail!(
                        "database protocol version {stored} is incompatible with node protocol {}",
                        aether_types::PROTOCOL_VERSION
                    );
                }
            }
            Some(_) => bail!("database protocol version marker is malformed"),
            None if storage.iterator(CF_BLOCKS)?.next().is_some() => {
                bail!("refusing non-empty legacy database without a protocol version marker")
            }
            None => storage.put(
                CF_METADATA,
                META_PROTOCOL_VERSION,
                &aether_types::PROTOCOL_VERSION.to_le_bytes(),
            )?,
        }
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
            Ok(None) => {
                // The consensus committee is trusted genesis input. Seed the
                // native staking ledger from it so canonical slash evidence
                // has economic state to update on a fresh node.
                let mut state = StakingState::new();
                for (address, stake) in consensus.validator_addresses_and_stakes_at_epoch(0) {
                    if stake == 0 || state.get_validator(&address).is_some() {
                        continue;
                    }
                    state.total_staked = state
                        .total_staked
                        .checked_add(stake)
                        .context("genesis staking total overflow")?;
                    state.validators.push(StakingValidator {
                        address,
                        staked_amount: stake,
                        delegated_amount: 0,
                        commission_rate: 0,
                        reward_address: address,
                        is_active: true,
                        jailed_until: None,
                        slash_count: 0,
                    });
                }
                state
            }
            Err(error) => {
                return Err(error).context("failed to read persisted staking state");
            }
        };
        let slashed_offenses: HashSet<(Address, Slot)> = ledger
            .storage()
            .get(CF_METADATA, META_SLASHED_OFFENSES)?
            .map(|bytes| bincode::deserialize(&bytes))
            .transpose()
            .context("failed to deserialize persisted slash offense set")?
            .unwrap_or_default();
        let mut pending_slash_evidence: HashMap<(Address, Slot), SlashEvidence> = ledger
            .storage()
            .get(CF_METADATA, META_PENDING_SLASH_EVIDENCE)?
            .map(|bytes| bincode::deserialize(&bytes))
            .transpose()
            .context("failed to deserialize persisted pending slash evidence")?
            .unwrap_or_default();
        ensure!(
            pending_slash_evidence.len() <= MAX_PENDING_SLASH_EVIDENCE,
            "persisted pending slash evidence exceeds the bounded queue limit"
        );
        let previous_pending_count = pending_slash_evidence.len();
        pending_slash_evidence.retain(|key, _| !slashed_offenses.contains(key));
        if pending_slash_evidence.len() != previous_pending_count {
            ledger.storage().put(
                CF_METADATA,
                META_PENDING_SLASH_EVIDENCE,
                &bincode::serialize(&pending_slash_evidence)?,
            )?;
        }

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
            slashed_offenses,
            detected_offenses: HashSet::new(),
            pending_slash_evidence,
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

        // A node that is missing blocks (orphans buffered) would only build a
        // stale fork, so it catches up first.
        if self.orphan_count == 0 {
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

    /// Perform non-consensus maintenance at an epoch boundary. Consensus state
    /// changes such as rewards and unbonding completions are applied by block
    /// execution, never by this wall-clock callback.
    fn process_epoch_transition(&mut self, new_epoch: u64) -> Result<()> {
        let _span = tracing::info_span!("epoch_transition", epoch = new_epoch).entered();

        let slot = self.consensus.current_slot();
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

        // Prune transient fork/vote state. Canonical slash offense IDs are
        // persisted and retained so old evidence cannot be replayed after restart.
        let finalized = self.consensus.finalized_slot();
        self.fork_choice.prune_before(finalized);
        self.slashing_detector.prune_before(finalized);
        self.committed_at_slot.retain(|&slot, _| slot >= finalized);
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
        slash_evidence: &[SlashEvidence],
        context: BlockContext,
    ) -> Result<ExecutedBlock> {
        let (parent_overlay, tree, mut fee_market) = self
            .parent_state(parent_hash)
            .ok_or_else(|| anyhow::anyhow!("parent state unavailable for {parent_hash:?}"))?;
        let (mut receipts, mut overlay, mut tree) = self.ledger.apply_block_on(
            parent_overlay,
            tree,
            transactions,
            Some(self.chain_config.chain.chain_id_numeric),
            context,
        )?;
        let mut branch_staking_state = self
            .pending
            .get(parent_hash)
            .map(|pending| pending.staking_state.clone())
            .unwrap_or_else(|| self.staking_state.clone());
        let mut branch_slashed_offenses = self
            .pending
            .get(parent_hash)
            .map(|pending| pending.slashed_offenses.clone())
            .unwrap_or_else(|| self.slashed_offenses.clone());
        let slash_events = self.apply_slash_evidence_to_state(
            slash_evidence,
            context.slot,
            &mut branch_staking_state,
            &mut branch_slashed_offenses,
        );
        let completed_unbondings = branch_staking_state.complete_unbonding(context.slot);
        // Epoch emissions are branch-local block effects. A child block that
        // crosses one or more epoch boundaries carries every crossed reward
        // exactly once in its overlay; ticks and restarts never mutate balances.
        let parent_slot = self
            .block_slot(parent_hash)
            .ok_or_else(|| anyhow::anyhow!("parent slot unavailable for {parent_hash:?}"))?;
        let epoch_slots = self.chain_config.chain.epoch_slots.max(1);
        let first_reward_epoch = parent_slot / epoch_slots + 1;
        let last_reward_epoch = context.slot / epoch_slots;
        let mut credits = completed_unbondings;
        if first_reward_epoch <= last_reward_epoch {
            let supply = self.chain_config.tokens.swr_initial_supply;
            for epoch in first_reward_epoch..=last_reward_epoch {
                let total_stake = self.consensus.total_stake_at_epoch(epoch);
                if total_stake == 0 {
                    continue;
                }
                let boundary_slot = epoch.saturating_mul(epoch_slots);
                let emission = self.emission_schedule.epoch_emission(boundary_slot, supply);
                for (address, stake) in self
                    .consensus
                    .validator_addresses_and_stakes_at_epoch(epoch)
                {
                    let share = mul_div(emission, stake, total_stake);
                    if share > 0 {
                        credits.push((address, share));
                    }
                }
            }
        }
        self.ledger
            .apply_account_credits(&mut overlay, &mut tree, &credits)?;
        // A failed transaction has no debit in the ledger and therefore cannot
        // fund a proposer reward, treasury credit, or burn. Successful
        // transactions pay their signed flat fee and reserve their declared gas.
        let (total_fees, gas_used) = transactions.iter().zip(&receipts).try_fold(
            (0u128, 0u64),
            |(fees, gas), (tx, receipt)| -> Result<_> {
                if !matches!(receipt.status, TransactionStatus::Success) {
                    return Ok((fees, gas));
                }
                Ok((
                    fees.checked_add(tx.fee)
                        .context("block fee total overflow")?,
                    gas.checked_add(tx.gas_limit)
                        .context("block gas total overflow")?,
                ))
            },
        )?;
        let fee_result = fee_market.process_block(gas_used, total_fees);
        self.ledger.apply_fee_distribution(
            &mut overlay,
            &mut tree,
            proposer,
            fee_result.proposer_reward,
            fee_result.burned,
            fee_result.treasury_fee,
        )?;
        let state_root = overlay.state_root;
        for receipt in &mut receipts {
            receipt.state_root = state_root;
        }
        let receipts_root = compute_receipts_root(&receipts);
        Ok(ExecutedBlock {
            receipts,
            overlay,
            tree,
            fee_market,
            staking_state: branch_staking_state,
            slash_events,
            slashed_offenses: branch_slashed_offenses,
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
                staking_state: executed.staking_state,
                slash_events: executed.slash_events,
                slashed_offenses: executed.slashed_offenses,
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
        let evidence = self.next_slash_evidence();
        self.produce_block_with_evidence(slot, &evidence)
    }

    fn next_slash_evidence(&self) -> Vec<SlashEvidence> {
        let mut pending: Vec<_> = self
            .pending_slash_evidence
            .iter()
            .filter(|(key, _)| !self.slashed_offenses.contains(key))
            .map(|(key, evidence)| (*key, evidence.clone()))
            .collect();
        pending.sort_by_key(|((address, slot), _)| (address.as_bytes().to_vec(), *slot));
        pending
            .into_iter()
            .take(MAX_PENDING_SLASH_EVIDENCE)
            .map(|(_, evidence)| evidence)
            .collect()
    }

    /// Propose a block carrying slash evidence. Evidence is applied only as
    /// branch state and becomes canonical when the block finalizes.
    pub fn propose_block_with_slash_evidence(
        &mut self,
        slot: Slot,
        slash_evidence: Vec<SlashEvidence>,
    ) -> Result<()> {
        self.produce_block_with_evidence(slot, &slash_evidence)
    }

    fn produce_block_with_evidence(
        &mut self,
        slot: Slot,
        slash_evidence: &[SlashEvidence],
    ) -> Result<()> {
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

        let executed = self.execute_on_parent(
            &parent_hash,
            &proposer,
            &transactions,
            slash_evidence,
            context,
        )?;
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
        block.slash_evidence = slash_evidence.to_vec();

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
        let my_stake = self.consensus.validator_stake_at_slot(&voter_addr, slot);

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

        // Already known (pending, finalized or cached)
        if self.pending.contains_key(&block_hash)
            || block_hash == self.finalized_hash
            || self.blocks_by_hash.contains_key(&block_hash)
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
        // State we can execute on: the finalized tip or a pending block.
        let parent_state_known =
            parent_hash == self.finalized_hash || self.pending.contains_key(&parent_hash);
        // Header we can validate against (also older finalized blocks).
        let parent_header_known = parent_state_known
            || parent_hash == H256::zero()
            || self.blocks_by_hash.contains_key(&parent_hash);
        if !parent_header_known {
            // Orphan: buffer until its parent arrives (catch-up requests
            // fetch missing ancestors).
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

        // A valid block whose parent state we no longer have extends a branch
        // that lost to finality (or genesis after the chain moved on).
        if !parent_state_known {
            tracing::debug!(
                slot = block.header.slot,
                "Ignoring block on a branch that conflicts with finality"
            );
            return Ok(());
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
            &block.slash_evidence,
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
            voted_stake = voted_stake
                .saturating_add(self.consensus.validator_stake_at_slot(&addr, agg_vote.slot));
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
        let total_stake = self.consensus.total_stake_at_slot(agg_vote.slot);
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

        // Record only votes signed by the BLS key registered to this
        // validator. This keeps local evidence generation aligned with the
        // proof verifier and prevents unauthenticated votes entering it.
        let mut vote_message = Vec::with_capacity(40);
        vote_message.extend_from_slice(vote.block_hash.as_bytes());
        vote_message.extend_from_slice(&vote.slot.to_le_bytes());
        let registered_bls_key = self.consensus.get_bls_pubkey(&validator_address);
        let valid_bls_vote = registered_bls_key.as_ref().is_some_and(|key| {
            matches!(
                aether_crypto_bls::keypair::verify(key, &vote_message, vote.signature.as_bytes(),),
                Ok(true)
            )
        });
        let proof = if valid_bls_vote {
            self.slashing_detector.record_vote(
                validator_address,
                PublicKey::from_bytes(registered_bls_key.expect("checked above")),
                vote.slot,
                vote.block_hash,
                vote.signature.clone(),
            )
        } else {
            None
        };

        if let Some(proof) = proof {
            // This local observation may not yet be part of a finalized block.
            // Keep the live diagnostic weight responsive, but leave economic
            // staking state and canonical offense dedup branch-local until the
            // proof is included in a finalized block.
            let offense_key = (proof.validator, vote.slot);
            if self.detected_offenses.insert(offense_key) {
                let rate_bps = slash_verify::slash_rate_bps(&proof.proof_type);
                self.consensus
                    .slash_validator(&proof.validator, u128::from(rate_bps));
                tracing::warn!(
                    validator = ?proof.validator,
                    slot = vote.slot,
                    rate_bps,
                    "Double-sign detected locally; economic slash awaits finalized block evidence"
                );
            } else {
                tracing::debug!(
                    validator = ?proof.validator,
                    slot = vote.slot,
                    "Double-sign already slashed for this (validator, slot) — skipping duplicate"
                );
            }

            let offense_key = (proof.validator, proof.vote1.slot);
            if !self.slashed_offenses.contains(&offense_key)
                && !self.pending_slash_evidence.contains_key(&offense_key)
                && self.pending_slash_evidence.len() < MAX_PENDING_SLASH_EVIDENCE
            {
                let to_evidence_vote = |vote: &slash_verify::Vote| aether_types::SlashVote {
                    slot: vote.slot,
                    block_hash: vote.block_hash,
                    validator: vote.validator,
                    validator_pubkey: vote.validator_pubkey.clone(),
                    signature: vote.signature.clone(),
                };
                let mut queued_evidence = self.pending_slash_evidence.clone();
                queued_evidence.insert(
                    offense_key,
                    SlashEvidence {
                        validator: proof.validator,
                        slash_rate_bps: slash_verify::slash_rate_bps(&proof.proof_type),
                        reason: "double_sign".to_string(),
                        vote1: Some(to_evidence_vote(&proof.vote1)),
                        vote2: Some(to_evidence_vote(&proof.vote2)),
                        evidence_type: Some(aether_types::SlashEvidenceType::DoubleSign),
                    },
                );
                self.ledger.storage().put(
                    CF_METADATA,
                    META_PENDING_SLASH_EVIDENCE,
                    &bincode::serialize(&queued_evidence)?,
                )?;
                self.pending_slash_evidence = queued_evidence;
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

        // Evaluate newly finalized slots for metric reporting; inspect
        // recent slots; bounded to avoid CPU spikes after long gaps.
        let end = current_slot.min(last_finalized.saturating_add(100));
        for slot in last_finalized..=end {
            if self.consensus.check_finality(slot) {
                CONSENSUS_METRICS.slots_finalized.inc();
            }
        }

        let finalized_slot = self.consensus.finalized_slot();
        let target = match self.consensus.finalized_block() {
            Some(hash) => Some(hash),
            // Engines that only report a slot: map it onto our head chain,
            // but only once it moves past what we already persisted (slot 0
            // with nothing finalized must not finalize a slot-0 proposal).
            None if finalized_slot > self.finalized_tip_slot => {
                self.pending_on_head_chain_at(finalized_slot)
            }
            None => None,
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
        let Some(pb) = self.pending.get(&block_hash).cloned() else {
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
        // Persist the exact branch state whose unbonding returns are present in
        // the finalized account overlay.
        Self::persist_staking_value_to_batch(&pb.staking_state, &mut extra)?;
        extra.put(
            CF_METADATA,
            META_SLASHED_OFFENSES.to_vec(),
            bincode::serialize(&pb.slashed_offenses)?,
        );
        let mut remaining_slash_evidence = self.pending_slash_evidence.clone();
        remaining_slash_evidence.retain(|offense, _| !pb.slashed_offenses.contains(offense));
        extra.put(
            CF_METADATA,
            META_PENDING_SLASH_EVIDENCE.to_vec(),
            bincode::serialize(&remaining_slash_evidence)?,
        );

        self.ledger.commit_pending(&pb.overlay, &pb.tree, extra)?;
        self.pending.remove(&block_hash);
        self.slashed_offenses = pb.slashed_offenses.clone();
        self.pending_slash_evidence = remaining_slash_evidence;
        for (address, _, rate_bps) in &pb.slash_events {
            self.consensus.slash_validator(address, *rate_bps);
        }
        self.staking_state = pb.staking_state.clone();
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

    /// Apply slash evidence to the branch state while executing its block.
    /// Consensus weight changes are deferred until the block finalizes.
    fn apply_slash_evidence_to_state(
        &self,
        evidence_list: &[SlashEvidence],
        block_slot: Slot,
        staking_state: &mut StakingState,
        slashed_offenses: &mut HashSet<(Address, Slot)>,
    ) -> Vec<(Address, Slot, u128)> {
        let mut events = Vec::new();
        for evidence in evidence_list {
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

            // BLS signatures prove that the supplied key signed the votes,
            // but the key must also be the one registered to this validator
            // address or an attacker could slash a victim with their own key.
            let Some(registered_bls_key) = self.consensus.get_bls_pubkey(&evidence.validator)
            else {
                tracing::warn!(
                    validator = ?evidence.validator,
                    "Slash rejected — validator has no registered BLS key"
                );
                continue;
            };
            if v1.validator_pubkey.as_bytes() != registered_bls_key.as_slice()
                || v2.validator_pubkey.as_bytes() != registered_bls_key.as_slice()
            {
                tracing::warn!(
                    validator = ?evidence.validator,
                    "Slash rejected — proof BLS key does not match registered validator key"
                );
                continue;
            }

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
            if slashed_offenses.contains(&offense_key) {
                tracing::debug!(
                    validator = ?evidence.validator,
                    slot = v1.slot,
                    reason = %evidence.reason,
                    "Slash already applied for this (validator, slot) — skipping block evidence"
                );
                continue;
            }

            let rate_bps = slash_verify::slash_rate_bps(&proof.proof_type);
            match staking_state.slash(evidence.validator, u128::from(rate_bps), block_slot) {
                Ok(slashed) => {
                    slashed_offenses.insert(offense_key);
                    events.push((evidence.validator, offense_key.1, u128::from(rate_bps)));
                    tracing::warn!(
                        validator = ?evidence.validator,
                        rate_bps,
                        slashed,
                        reason = %evidence.reason,
                        "Slash applied in speculative block state"
                    )
                }
                Err(e) => tracing::warn!(
                    validator = ?evidence.validator,
                    reason = %evidence.reason,
                    err = %e,
                    "Slash skipped (block evidence)"
                ),
            }
        }
        events
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

    pub fn chain_config(&self) -> &ChainConfig {
        &self.chain_config
    }

    pub fn current_slot(&self) -> Slot {
        self.consensus.current_slot()
    }

    pub fn finalized_slot(&self) -> Slot {
        self.consensus.finalized_slot().max(self.finalized_tip_slot)
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
        Self::persist_staking_value_to_batch(&self.staking_state, &mut batch)?;
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

    fn persist_staking_value_to_batch(
        staking_state: &StakingState,
        batch: &mut StorageBatch,
    ) -> Result<()> {
        let bytes =
            bincode::serialize(staking_state).context("failed to serialize staking state")?;
        batch.put(CF_STAKING, b"staking_state".to_vec(), bytes);
        Ok(())
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
