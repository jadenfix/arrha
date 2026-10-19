use aether_types::{BlockHeader, Slot, H256};
use std::collections::BTreeMap;

/// Stores verified finalized headers for the light client.
///
/// Only keeps the last `max_headers` to bound memory usage.
pub struct HeaderStore {
    headers: BTreeMap<Slot, BlockHeader>,
    max_headers: usize,
}

impl HeaderStore {
    pub fn new(max_headers: usize) -> Self {
        HeaderStore {
            headers: BTreeMap::new(),
            max_headers,
        }
    }

    /// Store a verified header.
    pub fn insert(&mut self, header: BlockHeader) {
        self.headers.insert(header.slot, header);

        // Evict oldest if over capacity
        while self.headers.len() > self.max_headers {
            self.headers.pop_first();
        }
    }

    /// Get a header by slot.
    pub fn get(&self, slot: Slot) -> Option<&BlockHeader> {
        self.headers.get(&slot)
    }

    /// Get the latest stored header.
    pub fn latest(&self) -> Option<&BlockHeader> {
        self.headers.values().next_back()
    }

    /// Get the state root at a given slot.
    pub fn state_root_at(&self, slot: Slot) -> Option<H256> {
        self.headers.get(&slot).map(|h| h.state_root)
    }

    pub fn len(&self) -> usize {
        self.headers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.headers.is_empty()
    }
}
