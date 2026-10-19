use aether_types::{Block, Slot, H256};
use std::time::{Duration, Instant};

/// Maximum blocks to buffer during sync to prevent OOM.
const MAX_SYNC_BUFFER: usize = 1024;

/// Maximum number of slots to request per sync batch.
const SYNC_BATCH_SIZE: u64 = 64;

/// After this duration of no progress, sync transitions to Stalled.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Sync state machine for catching up to the network tip.
#[derive(Debug, Clone, PartialEq)]
pub enum SyncState {
    /// Fully synced with the network.
    Synced,
    /// Currently syncing from `from_slot` to `target_slot`.
    Syncing { from_slot: Slot, target_slot: Slot },
    /// Sync stalled (no progress for too long).
    Stalled,
}

/// Manages block synchronization for nodes that are behind the network.
///
/// When a node detects it is behind the network tip by more than
/// `sync_threshold` slots, it enters syncing mode: it requests missing
/// blocks in batches via gossipsub, buffers responses, and applies them
/// in slot order once each batch is complete.
pub struct SyncManager {
    state: SyncState,
    /// How many slots behind before we consider ourselves "out of sync".
    sync_threshold: u64,
    /// Blocks received during sync (buffered for ordered processing).
    sync_buffer: Vec<Block>,
    /// Highest slot we have successfully applied so far.
    next_expected_slot: Slot,
    /// Parent hash expected for the next block (for chain continuity validation).
    expected_parent_hash: Option<H256>,
    /// Last time we made progress (received a useful block).
    last_progress: Option<Instant>,
    /// End of the current batch being requested.
    current_batch_end: Slot,
    /// Count of blocks successfully applied during this sync session.
    blocks_applied: u64,
}

impl SyncManager {
    pub fn new(sync_threshold: u64) -> Self {
        SyncManager {
            state: SyncState::Synced,
            sync_threshold,
            sync_buffer: Vec::new(),
            next_expected_slot: 0,
            expected_parent_hash: None,
            last_progress: None,
            current_batch_end: 0,
            blocks_applied: 0,
        }
    }

    /// Check if we need to sync based on our latest slot vs network slot.
    pub fn check_sync_needed(&mut self, my_latest_slot: Slot, network_slot: Slot) -> bool {
        if network_slot > my_latest_slot.saturating_add(self.sync_threshold) {
            if !self.is_syncing() {
                self.next_expected_slot = my_latest_slot.saturating_add(1);
                self.current_batch_end = 0;
                self.last_progress = Some(Instant::now());
                self.blocks_applied = 0;
            }
            self.state = SyncState::Syncing {
                from_slot: my_latest_slot,
                target_slot: network_slot,
            };
            true
        } else {
            if matches!(self.state, SyncState::Syncing { .. }) {
                self.mark_synced();
            }
            false
        }
    }

    /// Get the current sync state.
    pub fn state(&self) -> &SyncState {
        &self.state
    }

    /// Check if we're currently syncing.
    pub fn is_syncing(&self) -> bool {
        matches!(self.state, SyncState::Syncing { .. })
    }

    /// Buffer a block received during sync. Returns false if buffer is full.
    pub fn buffer_block(&mut self, block: Block) -> bool {
        if self.sync_buffer.len() >= MAX_SYNC_BUFFER {
            return false;
        }
        // Track progress when we receive a block we need.
        self.last_progress = Some(Instant::now());
        self.sync_buffer.push(block);
        true
    }

    /// Drain buffered blocks that form a contiguous sequence starting at
    /// `next_expected_slot`. Returns them sorted by slot, validated for
    /// parent hash chain continuity. Blocks that don't continue the chain
    /// or fail parent hash checks are kept in the buffer.
    pub fn drain_ready(&mut self) -> Vec<Block> {
        if self.sync_buffer.is_empty() {
            return Vec::new();
        }
        self.sync_buffer.sort_by_key(|b| b.header.slot);

        let mut ready = Vec::new();
        let mut remaining = Vec::new();

        for block in self.sync_buffer.drain(..) {
            if block.header.slot == self.next_expected_slot {
                // Validate parent hash chain continuity when we have an expected parent.
                if let Some(expected) = self.expected_parent_hash {
                    if block.header.parent_hash != expected
                        && block.header.parent_hash != H256::zero()
                    {
                        // Parent mismatch — keep in buffer (might be from a different fork).
                        remaining.push(block);
                        continue;
                    }
                }
                self.expected_parent_hash = Some(block.hash());
                self.next_expected_slot = self.next_expected_slot.saturating_add(1);
                ready.push(block);
            } else if block.header.slot > self.next_expected_slot {
                remaining.push(block);
            }
            // blocks with slot < next_expected_slot are duplicates, drop them
        }

        // Continue draining any that now form a contiguous sequence
        remaining.sort_by_key(|b| b.header.slot);
        while !remaining.is_empty() && remaining[0].header.slot == self.next_expected_slot {
            let block = &remaining[0];
            // Validate parent hash chain for continuation blocks too.
            if let Some(expected) = self.expected_parent_hash {
                if block.header.parent_hash != expected && block.header.parent_hash != H256::zero()
                {
                    break;
                }
            }
            let block = remaining.remove(0);
            self.expected_parent_hash = Some(block.hash());
            self.next_expected_slot = self.next_expected_slot.saturating_add(1);
            ready.push(block);
        }

        self.sync_buffer = remaining;
        ready
    }

    /// Get the next batch of slots to request from peers.
    /// Returns `None` if we're not syncing or waiting for the current batch.
    pub fn next_request(&mut self) -> Option<(Slot, Slot)> {
        let target = match self.state {
            SyncState::Syncing { target_slot, .. } => target_slot,
            _ => return None,
        };

        // Don't request if we're still waiting for the current batch
        if self.current_batch_end >= self.next_expected_slot && !self.sync_buffer.is_empty() {
            return None;
        }

        let from = self.next_expected_slot;
        if from > target {
            return None;
        }

        let to = (from + SYNC_BATCH_SIZE).min(target);
        self.current_batch_end = to;
        Some((from, to))
    }

    /// Check if sync has stalled (no progress for STALL_TIMEOUT).
    pub fn check_stalled(&mut self) -> bool {
        if let Some(last) = self.last_progress {
            if last.elapsed() >= STALL_TIMEOUT {
                self.state = SyncState::Stalled;
                return true;
            }
        }
        false
    }

    /// Reset stall state and retry from where we left off.
    pub fn retry_after_stall(&mut self, target_slot: Slot) {
        self.state = SyncState::Syncing {
            from_slot: self.next_expected_slot,
            target_slot,
        };
        self.current_batch_end = 0;
        self.last_progress = Some(Instant::now());
    }

    /// Mark sync as complete.
    pub fn mark_synced(&mut self) {
        self.state = SyncState::Synced;
        self.sync_buffer.clear();
        self.current_batch_end = 0;
        self.expected_parent_hash = None;
        self.blocks_applied = 0;
    }

    /// Set the expected parent hash for chain continuity validation.
    /// Should be called with the hash of the last locally-applied block
    /// before sync begins.
    pub fn set_expected_parent(&mut self, hash: H256) {
        self.expected_parent_hash = Some(hash);
    }

    /// Record that a synced block was successfully applied.
    pub fn record_applied(&mut self) {
        self.blocks_applied += 1;
    }

    /// Get the number of blocks applied during this sync session.
    pub fn blocks_applied(&self) -> u64 {
        self.blocks_applied
    }

    /// Get the range of slots we need to sync.
    pub fn sync_range(&self) -> Option<(Slot, Slot)> {
        match self.state {
            SyncState::Syncing {
                from_slot,
                target_slot,
            } => Some((from_slot, target_slot)),
            _ => None,
        }
    }

    /// Number of blocks currently buffered.
    pub fn buffer_len(&self) -> usize {
        self.sync_buffer.len()
    }

    /// The next slot we expect to receive.
    pub fn next_expected(&self) -> Slot {
        self.next_expected_slot
    }
}
