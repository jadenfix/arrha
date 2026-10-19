use crate::proof::{internal_hash, leaf_hash, MerkleProof};
use aether_types::{Address, H256};
use std::collections::HashMap;

/// Sparse Merkle Tree with 160-bit depth (matching Address = 20 bytes).
///
/// Only non-empty leaves are stored. Hashes are computed by recursively
/// partitioning a sorted leaf array by address bits at each tree level.
/// Empty subtrees use precomputed default hashes.
///
/// Key optimization: leaves are sorted by address once, then partitioned
/// via binary search (`partition_point`) at each level — zero vector
/// cloning or per-level allocation compared to the naive approach.
#[derive(Clone, Debug)]
pub struct SparseMerkleTree {
    root: H256,
    /// True when leaves were changed via `set_leaf` and `root` is stale.
    dirty: bool,
    leaves: HashMap<Address, H256>,
    /// defaults[0] = empty leaf hash, defaults[d] = hash of empty subtree of height d
    defaults: Vec<H256>,
    depth: usize,
}

impl SparseMerkleTree {
    pub fn new() -> Self {
        let depth = 160;
        let defaults = precompute_defaults(depth);
        let root = defaults[depth];

        SparseMerkleTree {
            root,
            dirty: false,
            leaves: HashMap::new(),
            defaults,
            depth,
        }
    }

    pub fn update(&mut self, key: Address, value_hash: H256) {
        self.leaves.insert(key, value_hash);
        self.root = self.compute_root();
        self.dirty = false;
    }

    /// Set a leaf without recomputing the root.
    ///
    /// Recomputing the root costs O(leaves * depth) hashes, so block execution
    /// batches all leaf changes with `set_leaf` and calls [`Self::recompute`]
    /// once per block. `root()` stays correct in between (it recomputes on
    /// demand while dirty), it is just slower.
    pub fn set_leaf(&mut self, key: Address, value_hash: H256) {
        self.leaves.insert(key, value_hash);
        self.dirty = true;
    }

    /// Recompute and cache the root after one or more `set_leaf` calls.
    pub fn recompute(&mut self) {
        if self.dirty {
            self.root = self.compute_root();
            self.dirty = false;
        }
    }

    pub fn delete(&mut self, key: &Address) {
        self.leaves.remove(key);
        self.root = self.compute_root();
        self.dirty = false;
    }

    pub fn get(&self, key: &Address) -> Option<H256> {
        self.leaves.get(key).copied()
    }

    pub fn root(&self) -> H256 {
        if self.dirty {
            self.compute_root()
        } else {
            self.root
        }
    }

    /// Generate a Merkle proof for a key.
    ///
    /// Returns siblings in leaf-to-root order (index 0 = deepest sibling).
    pub fn prove(&self, key: &Address) -> MerkleProof {
        let value_hash = self.leaves.get(key).copied();

        // Sort leaves by address bytes for efficient partitioning
        let mut sorted: Vec<(Address, H256)> = self
            .leaves
            .iter()
            .map(|(addr, vh)| (*addr, leaf_hash(addr, vh)))
            .collect();
        sorted.sort_unstable_by(|(a, _), (b, _)| a.as_bytes().cmp(b.as_bytes()));

        // Collect siblings top-to-bottom, then reverse to get leaf-to-root order
        let mut siblings = self.collect_siblings_sorted(key, &sorted, 0);
        siblings.reverse();

        MerkleProof::new(*key, value_hash, self.root(), siblings)
    }

    /// Collect sibling hashes top-to-bottom using sorted slice partitioning.
    fn collect_siblings_sorted(
        &self,
        target: &Address,
        leaves: &[(Address, H256)],
        bit_index: usize,
    ) -> Vec<H256> {
        if bit_index >= self.depth {
            return Vec::new();
        }

        let split = partition_by_bit(leaves, bit_index);
        let target_goes_right = get_bit(target, bit_index);

        let (same_side, other_side) = if !target_goes_right {
            (&leaves[..split], &leaves[split..])
        } else {
            (&leaves[split..], &leaves[..split])
        };

        let remaining_height = self.depth - bit_index - 1;
        let sibling_hash = self.subtree_hash_sorted(other_side, bit_index + 1, remaining_height);

        let mut siblings = vec![sibling_hash];
        siblings.extend(self.collect_siblings_sorted(target, same_side, bit_index + 1));
        siblings
    }

    fn compute_root(&self) -> H256 {
        if self.leaves.is_empty() {
            return self.defaults[self.depth];
        }

        // Sort leaves once by address bytes — binary search partitions at each level
        let mut sorted: Vec<(Address, H256)> = self
            .leaves
            .iter()
            .map(|(addr, vh)| (*addr, leaf_hash(addr, vh)))
            .collect();
        sorted.sort_unstable_by(|(a, _), (b, _)| a.as_bytes().cmp(b.as_bytes()));

        self.subtree_hash_sorted(&sorted, 0, self.depth)
    }

    /// Compute the hash of a subtree from a sorted slice of leaves.
    ///
    /// Uses `partition_point` (binary search) to split at each level instead of
    /// cloning vectors. This eliminates all per-level allocations.
    fn subtree_hash_sorted(
        &self,
        leaves: &[(Address, H256)],
        bit_index: usize,
        height: usize,
    ) -> H256 {
        if leaves.is_empty() {
            return self.defaults[height];
        }

        if height == 0 {
            return leaves[0].1;
        }

        // Binary search: all leaves with bit=0 at bit_index come first (sorted order)
        let split = partition_by_bit(leaves, bit_index);

        let left_hash = self.subtree_hash_sorted(&leaves[..split], bit_index + 1, height - 1);
        let right_hash = self.subtree_hash_sorted(&leaves[split..], bit_index + 1, height - 1);

        internal_hash(&left_hash, &right_hash)
    }
}

impl Default for SparseMerkleTree {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the bit at `bit_index` from an address (MSB-first ordering).
/// bit 0 = MSB of byte 0, bit 7 = LSB of byte 0, bit 8 = MSB of byte 1, etc.
#[inline]
fn get_bit(addr: &Address, bit_index: usize) -> bool {
    let byte_idx = bit_index / 8;
    let bit_offset = 7 - (bit_index % 8);
    (addr.as_bytes()[byte_idx] >> bit_offset) & 1 == 1
}

/// Find the partition point in a sorted slice where bit at `bit_index` goes from 0 to 1.
///
/// Requires that `leaves` is sorted by address bytes (MSB-first), which guarantees
/// all bit-0 entries precede all bit-1 entries within any recursive sub-slice that
/// shares a common prefix up to `bit_index`.
#[inline]
fn partition_by_bit(leaves: &[(Address, H256)], bit_index: usize) -> usize {
    let byte_idx = bit_index / 8;
    let bit_offset = 7 - (bit_index % 8);
    leaves.partition_point(|(addr, _)| (addr.as_bytes()[byte_idx] >> bit_offset) & 1 == 0)
}

/// Precompute default hashes.
/// defaults[0] = SHA256(0x00) (empty leaf, domain-separated)
/// defaults[d] = internal_hash(defaults[d-1], defaults[d-1])
fn precompute_defaults(depth: usize) -> Vec<H256> {
    let mut defaults = Vec::with_capacity(depth + 1);
    // Domain-separated empty leaf: SHA256(0x00) — not raw zero
    let empty_leaf = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update([0x00]); // Leaf domain separator with no key/value
        H256::from(<[u8; 32]>::from(h.finalize()))
    };
    defaults.push(empty_leaf);
    for _ in 1..=depth {
        // Safety: we just pushed at least one element above
        let prev = defaults[defaults.len() - 1];
        defaults.push(internal_hash(&prev, &prev));
    }
    defaults
}
