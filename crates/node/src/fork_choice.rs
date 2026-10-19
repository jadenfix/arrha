use aether_types::{Slot, H256};
use std::collections::{HashMap, HashSet};

/// Maximum number of candidate blocks tracked per slot.
/// Prevents OOM from an attacker spamming unique block hashes for a single slot.
const MAX_CANDIDATES_PER_SLOT: usize = 16;

/// Simple fork choice: for each slot, track all candidate blocks and
/// select the canonical one based on finality or first-seen ordering.
pub struct ForkChoice {
    /// All known blocks per slot (can have multiple competing blocks).
    candidates: HashMap<Slot, Vec<H256>>,
    /// The chosen canonical block per slot.
    canonical: HashMap<Slot, H256>,
    /// Finalized blocks (immutable once set).
    finalized: HashMap<Slot, H256>,
    /// Slots whose canonical block has been committed to storage.
    /// Once committed, the canonical block for that slot is locked —
    /// a late-arriving fork cannot switch it without a full state rollback
    /// (which we don't support). This prevents state corruption from
    /// applying two competing blocks' overlays at the same slot.
    committed: HashSet<Slot>,
}

impl Default for ForkChoice {
    fn default() -> Self {
        Self::new()
    }
}

impl ForkChoice {
    pub fn new() -> Self {
        ForkChoice {
            candidates: HashMap::new(),
            canonical: HashMap::new(),
            finalized: HashMap::new(),
            committed: HashSet::new(),
        }
    }

    /// Record a new block candidate for a slot. Returns true if this is a new fork
    /// (competing block for an already-occupied slot).
    pub fn add_block(&mut self, slot: Slot, block_hash: H256) -> bool {
        // Reject new blocks for finalized or committed slots — once state is
        // committed to storage, switching canonical blocks would corrupt state
        // (both overlays applied without rollback).
        if self.finalized.contains_key(&slot) || self.committed.contains(&slot) {
            return false;
        }

        let candidates = self.candidates.entry(slot).or_default();
        if candidates.contains(&block_hash) {
            return false; // Already known
        }
        if candidates.len() >= MAX_CANDIDATES_PER_SLOT {
            return false; // Prevent OOM from excessive candidates per slot
        }
        let is_fork = !candidates.is_empty();
        candidates.push(block_hash);

        if is_fork {
            tracing::warn!(
                slot,
                block = ?block_hash,
                candidates = candidates.len(),
                "fork detected — competing block at same slot"
            );
        }

        // Deterministic tiebreak: prefer lower hash (compare raw bytes)
        match self.canonical.entry(slot) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(block_hash);
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                if block_hash.as_bytes() < e.get().as_bytes() {
                    e.insert(block_hash);
                }
            }
        }

        is_fork
    }

    /// Get the canonical block for a slot.
    pub fn canonical_block(&self, slot: Slot) -> Option<H256> {
        self.finalized
            .get(&slot)
            .or_else(|| self.canonical.get(&slot))
            .copied()
    }

    /// Finalize a block at a slot. Only blocks that are known candidates
    /// (or already canonical) can be finalized.
    pub fn finalize(&mut self, slot: Slot, block_hash: H256) -> bool {
        // Only finalize blocks we've actually seen
        let is_known = self
            .candidates
            .get(&slot)
            .is_some_and(|c| c.contains(&block_hash));
        let is_canonical = self.canonical.get(&slot) == Some(&block_hash);

        if !is_known && !is_canonical {
            return false; // Reject unknown block hash
        }

        tracing::debug!(slot, block = ?block_hash, "fork choice finalized");
        self.finalized.insert(slot, block_hash);
        self.canonical.insert(slot, block_hash);
        true
    }

    /// Check if a slot has competing blocks (a fork).
    pub fn has_fork(&self, slot: Slot) -> bool {
        self.candidates.get(&slot).is_some_and(|c| c.len() > 1)
    }

    /// Get all candidate blocks for a slot.
    pub fn candidates_for(&self, slot: Slot) -> &[H256] {
        self.candidates.get(&slot).map_or(&[], |v| v.as_slice())
    }

    /// Check if a slot is finalized.
    pub fn is_finalized(&self, slot: Slot) -> bool {
        self.finalized.contains_key(&slot)
    }

    /// Mark a slot as having its canonical block's state committed to storage.
    /// After this, no fork-choice reorg is allowed for this slot.
    pub fn mark_committed(&mut self, slot: Slot) {
        self.committed.insert(slot);
    }

    /// Prune data for slots before `min_slot` to bound memory.
    pub fn prune_before(&mut self, min_slot: Slot) {
        self.candidates.retain(|&s, _| s >= min_slot);
        self.canonical.retain(|&s, _| s >= min_slot);
        self.finalized.retain(|&s, _| s >= min_slot);
        self.committed.retain(|s| *s >= min_slot);
    }
}
