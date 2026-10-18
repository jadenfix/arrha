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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_leaf_then_recompute_matches_eager_updates() {
        let mut eager = SparseMerkleTree::new();
        let mut lazy = SparseMerkleTree::new();
        for i in 0..20u8 {
            let key = Address::from([i; 20]);
            let value = H256::from([i.wrapping_mul(7); 32]);
            eager.update(key, value);
            lazy.set_leaf(key, value);
        }
        // root() is correct while dirty, and recompute() caches the same value.
        assert_eq!(lazy.root(), eager.root());
        lazy.recompute();
        assert_eq!(lazy.root(), eager.root());
    }

    #[test]
    fn test_empty_tree() {
        let tree = SparseMerkleTree::new();
        assert_ne!(tree.root(), H256::zero());
    }

    #[test]
    fn test_update_and_get() {
        let mut tree = SparseMerkleTree::new();
        let addr = Address::from_slice(&[1u8; 20]).unwrap();
        let value = H256::from_slice(&[2u8; 32]).unwrap();
        tree.update(addr, value);
        assert_eq!(tree.get(&addr), Some(value));
    }

    #[test]
    fn test_root_changes_on_update() {
        let mut tree = SparseMerkleTree::new();
        let root1 = tree.root();
        let addr = Address::from_slice(&[1u8; 20]).unwrap();
        tree.update(addr, H256::from_slice(&[2u8; 32]).unwrap());
        assert_ne!(root1, tree.root());
    }

    #[test]
    fn test_deterministic_root() {
        let mut t1 = SparseMerkleTree::new();
        let mut t2 = SparseMerkleTree::new();
        let addr = Address::from_slice(&[1u8; 20]).unwrap();
        let val = H256::from_slice(&[2u8; 32]).unwrap();
        t1.update(addr, val);
        t2.update(addr, val);
        assert_eq!(t1.root(), t2.root());
    }

    #[test]
    fn test_different_values_different_roots() {
        let mut t1 = SparseMerkleTree::new();
        let mut t2 = SparseMerkleTree::new();
        let addr = Address::from_slice(&[1u8; 20]).unwrap();
        t1.update(addr, H256::from_slice(&[1u8; 32]).unwrap());
        t2.update(addr, H256::from_slice(&[2u8; 32]).unwrap());
        assert_ne!(t1.root(), t2.root());
    }

    #[test]
    fn test_delete_restores_root() {
        let mut tree = SparseMerkleTree::new();
        let empty_root = tree.root();
        let addr = Address::from_slice(&[1u8; 20]).unwrap();
        tree.update(addr, H256::from_slice(&[2u8; 32]).unwrap());
        tree.delete(&addr);
        assert_eq!(tree.root(), empty_root);
    }

    #[test]
    fn test_proof_verification_inclusion() {
        let mut tree = SparseMerkleTree::new();
        let addr = Address::from_slice(&[1u8; 20]).unwrap();
        let value = H256::from_slice(&[2u8; 32]).unwrap();
        tree.update(addr, value);
        let proof = tree.prove(&addr);
        assert!(proof.verify(), "inclusion proof must verify");
        assert_eq!(proof.value_hash, Some(value));
    }

    #[test]
    fn test_proof_verification_exclusion() {
        let mut tree = SparseMerkleTree::new();
        let addr1 = Address::from_slice(&[1u8; 20]).unwrap();
        tree.update(addr1, H256::from_slice(&[2u8; 32]).unwrap());

        let addr2 = Address::from_slice(&[3u8; 20]).unwrap();
        let proof = tree.prove(&addr2);
        assert!(proof.verify(), "exclusion proof must verify");
        assert_eq!(proof.value_hash, None);
    }

    #[test]
    fn test_proof_fails_with_wrong_root() {
        let mut tree = SparseMerkleTree::new();
        let addr = Address::from_slice(&[1u8; 20]).unwrap();
        tree.update(addr, H256::from_slice(&[2u8; 32]).unwrap());
        let mut proof = tree.prove(&addr);
        proof.root = H256::from_slice(&[99u8; 32]).unwrap();
        assert!(!proof.verify());
    }

    #[test]
    fn test_proof_fails_with_tampered_sibling() {
        let mut tree = SparseMerkleTree::new();
        let addr = Address::from_slice(&[1u8; 20]).unwrap();
        tree.update(addr, H256::from_slice(&[2u8; 32]).unwrap());
        let mut proof = tree.prove(&addr);
        proof.siblings[0] = H256::from_slice(&[99u8; 32]).unwrap();
        assert!(!proof.verify());
    }

    #[test]
    fn test_multiple_keys() {
        let mut tree = SparseMerkleTree::new();
        for i in 0u8..10 {
            let mut ab = [0u8; 20];
            ab[0] = i;
            let mut vb = [0u8; 32];
            vb[0] = i + 100;
            tree.update(
                Address::from_slice(&ab).unwrap(),
                H256::from_slice(&vb).unwrap(),
            );
        }
        for i in 0u8..10 {
            let mut ab = [0u8; 20];
            ab[0] = i;
            let addr = Address::from_slice(&ab).unwrap();
            let proof = tree.prove(&addr);
            assert!(proof.verify(), "proof for key {} must verify", i);
        }
    }

    #[test]
    fn test_order_independence() {
        let mut t1 = SparseMerkleTree::new();
        let mut t2 = SparseMerkleTree::new();
        let a = Address::from_slice(&[1u8; 20]).unwrap();
        let b = Address::from_slice(&[2u8; 20]).unwrap();
        let va = H256::from_slice(&[10u8; 32]).unwrap();
        let vb = H256::from_slice(&[20u8; 32]).unwrap();
        t1.update(a, va);
        t1.update(b, vb);
        t2.update(b, vb);
        t2.update(a, va);
        assert_eq!(t1.root(), t2.root());
    }

    #[test]
    fn test_batch_updates_consistent() {
        let mut tree = SparseMerkleTree::new();
        for i in 0u8..5 {
            let mut ab = [0u8; 20];
            ab[0] = i;
            tree.update(
                Address::from_slice(&ab).unwrap(),
                H256::from_slice(&[i + 10; 32]).unwrap(),
            );
        }
        let root = tree.root();
        assert_ne!(root, SparseMerkleTree::new().root());
        // Root is stable across calls
        assert_eq!(tree.root(), root);
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn arb_address() -> impl Strategy<Value = Address> {
        prop::array::uniform20(any::<u8>()).prop_map(|b| Address::from_slice(&b).unwrap())
    }

    fn arb_h256() -> impl Strategy<Value = H256> {
        prop::array::uniform32(any::<u8>()).prop_map(|b| H256::from_slice(&b).unwrap())
    }

    proptest! {
        /// Any inserted key can be proven and the proof verifies.
        #[test]
        fn prove_after_insert_verifies(addr in arb_address(), val in arb_h256()) {
            let mut tree = SparseMerkleTree::new();
            tree.update(addr, val);
            let proof = tree.prove(&addr);
            prop_assert!(proof.verify(), "proof for inserted key must verify");
            prop_assert_eq!(proof.value_hash, Some(val));
        }

        /// Deletion restores the empty-tree root.
        #[test]
        fn delete_single_key_restores_root(addr in arb_address(), val in arb_h256()) {
            let mut tree = SparseMerkleTree::new();
            let empty_root = tree.root();
            tree.update(addr, val);
            tree.delete(&addr);
            prop_assert_eq!(tree.root(), empty_root);
        }

        /// Inserting two keys in either order produces the same root.
        #[test]
        fn order_independent(a in arb_address(), va in arb_h256(), b in arb_address(), vb in arb_h256()) {
            let mut t1 = SparseMerkleTree::new();
            let mut t2 = SparseMerkleTree::new();
            t1.update(a, va);
            t1.update(b, vb);
            t2.update(b, vb);
            t2.update(a, va);
            prop_assert_eq!(t1.root(), t2.root());
        }

        /// Proofs verify for every key in a multi-key tree.
        #[test]
        fn all_keys_provable(entries in prop::collection::vec((arb_address(), arb_h256()), 1..20)) {
            let mut tree = SparseMerkleTree::new();
            for (addr, val) in &entries {
                tree.update(*addr, *val);
            }
            for (addr, _val) in &entries {
                let proof = tree.prove(addr);
                prop_assert!(proof.verify(), "inclusion proof must verify for all inserted keys");
                // value_hash should be Some (key exists) — though if duplicate addrs,
                // the last value wins, so just check it's Some
                prop_assert!(proof.value_hash.is_some());
            }
        }

        /// Exclusion proofs verify for keys not in the tree.
        #[test]
        fn exclusion_proof_verifies(
            entries in prop::collection::vec((arb_address(), arb_h256()), 1..10),
            absent in arb_address(),
        ) {
            let mut tree = SparseMerkleTree::new();
            for (addr, val) in &entries {
                tree.update(*addr, *val);
            }
            // Only test if `absent` wasn't in entries
            if !entries.iter().any(|(a, _)| *a == absent) {
                let proof = tree.prove(&absent);
                prop_assert!(proof.verify(), "exclusion proof must verify");
                prop_assert_eq!(proof.value_hash, None);
            }
        }

        /// Tampered value hash causes proof verification failure.
        #[test]
        fn tampered_value_fails(addr in arb_address(), val in arb_h256(), tampered_val in arb_h256()) {
            prop_assume!(val != tampered_val);
            let mut tree = SparseMerkleTree::new();
            tree.update(addr, val);
            let mut proof = tree.prove(&addr);
            proof.value_hash = Some(tampered_val);
            prop_assert!(!proof.verify(), "tampered value hash must fail verification");
        }

        /// Updating a key to the same value is idempotent (root unchanged).
        #[test]
        fn idempotent_update(addr in arb_address(), val in arb_h256()) {
            let mut tree = SparseMerkleTree::new();
            tree.update(addr, val);
            let root1 = tree.root();
            tree.update(addr, val);
            prop_assert_eq!(tree.root(), root1);
        }

        /// Deleting all keys restores the empty root.
        #[test]
        fn delete_all_restores_empty(entries in prop::collection::vec((arb_address(), arb_h256()), 1..15)) {
            let mut tree = SparseMerkleTree::new();
            let empty_root = tree.root();
            let mut addrs = Vec::new();
            for (addr, val) in &entries {
                tree.update(*addr, *val);
                addrs.push(*addr);
            }
            // Deduplicate addresses (last update wins, so just delete each unique addr)
            addrs.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            addrs.dedup();
            for addr in &addrs {
                tree.delete(addr);
            }
            prop_assert_eq!(tree.root(), empty_root);
        }

        /// Insertion order doesn't matter for N keys (generalized order independence).
        #[test]
        fn insertion_order_independent(entries in prop::collection::vec((arb_address(), arb_h256()), 2..15)) {
            let mut forward = SparseMerkleTree::new();
            for (addr, val) in &entries {
                forward.update(*addr, *val);
            }
            let mut backward = SparseMerkleTree::new();
            for (addr, val) in entries.iter().rev() {
                backward.update(*addr, *val);
            }
            // Note: if there are duplicate keys, insertion order affects which value
            // is final, so roots may differ. That's expected. We only assert equality
            // when all keys are unique.
            let mut keys: Vec<_> = entries.iter().map(|(a, _)| *a).collect();
            keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            let unique = keys.windows(2).all(|w| w[0] != w[1]);
            if unique {
                prop_assert_eq!(forward.root(), backward.root());
            }
        }

        /// Proof siblings have correct length (depth = 160).
        #[test]
        fn proof_has_correct_depth(addr in arb_address(), val in arb_h256()) {
            let mut tree = SparseMerkleTree::new();
            tree.update(addr, val);
            let proof = tree.prove(&addr);
            prop_assert_eq!(proof.siblings.len(), 160);
        }

        /// Updating a value changes the root but keeps other proofs valid.
        #[test]
        fn update_value_changes_root(addr in arb_address(), v1 in arb_h256(), v2 in arb_h256()) {
            prop_assume!(v1 != v2);
            let mut tree = SparseMerkleTree::new();
            tree.update(addr, v1);
            let root1 = tree.root();
            tree.update(addr, v2);
            let root2 = tree.root();
            prop_assert_ne!(root1, root2);
            // Proof still verifies after update
            let proof = tree.prove(&addr);
            prop_assert!(proof.verify());
            prop_assert_eq!(proof.value_hash, Some(v2));
        }

        /// Tampered sibling at any depth causes verification failure.
        #[test]
        fn tampered_sibling_at_any_depth_fails(
            addr in arb_address(),
            val in arb_h256(),
            tamper_idx in 0usize..160,
            tamper_val in arb_h256(),
        ) {
            let mut tree = SparseMerkleTree::new();
            tree.update(addr, val);
            let mut proof = tree.prove(&addr);
            prop_assume!(proof.siblings[tamper_idx] != tamper_val);
            proof.siblings[tamper_idx] = tamper_val;
            prop_assert!(!proof.verify(), "tampered sibling at depth {} must fail", tamper_idx);
        }

        /// A proof is bound to the tree state — inserting another key invalidates it.
        #[test]
        fn proof_bound_to_state(
            addr_a in arb_address(),
            val_a in arb_h256(),
            addr_b in arb_address(),
            val_b in arb_h256(),
        ) {
            prop_assume!(addr_a != addr_b);
            let mut tree = SparseMerkleTree::new();
            tree.update(addr_a, val_a);
            let proof_before = tree.prove(&addr_a);
            prop_assert!(proof_before.verify());

            tree.update(addr_b, val_b);
            // Old proof has stale root — must not verify against new tree state
            prop_assert_ne!(proof_before.root, tree.root());
            // But a fresh proof from the updated tree does verify
            let proof_after = tree.prove(&addr_a);
            prop_assert!(proof_after.verify());
        }

        /// A valid proof for key A must not verify if the key is swapped to B.
        #[test]
        fn proof_not_transferable_to_other_key(
            addr_a in arb_address(),
            addr_b in arb_address(),
            val in arb_h256(),
        ) {
            prop_assume!(addr_a != addr_b);
            let mut tree = SparseMerkleTree::new();
            tree.update(addr_a, val);
            let mut proof = tree.prove(&addr_a);
            proof.key = addr_b;
            prop_assert!(!proof.verify(), "proof must not transfer to a different key");
        }

        /// Different (key, value) pairs produce different leaf hashes (second preimage resistance).
        #[test]
        fn distinct_leaves_distinct_hashes(
            k1 in arb_address(), v1 in arb_h256(),
            k2 in arb_address(), v2 in arb_h256(),
        ) {
            use crate::proof::leaf_hash;
            prop_assume!((k1, v1) != (k2, v2));
            prop_assert_ne!(
                leaf_hash(&k1, &v1),
                leaf_hash(&k2, &v2),
                "different (key, value) pairs should produce different leaf hashes"
            );
        }

        /// Keys differing in exactly one bit both produce valid proofs.
        #[test]
        fn bit_adjacent_keys_both_provable(
            base_addr in arb_address(),
            val_a in arb_h256(),
            val_b in arb_h256(),
            flip_bit in 0usize..160,
        ) {
            let mut bytes = *base_addr.as_bytes();
            let byte_idx = flip_bit / 8;
            let bit_offset = 7 - (flip_bit % 8);
            bytes[byte_idx] ^= 1 << bit_offset;
            let neighbor = Address::from_slice(&bytes).unwrap();
            prop_assume!(base_addr != neighbor);

            let mut tree = SparseMerkleTree::new();
            tree.update(base_addr, val_a);
            tree.update(neighbor, val_b);

            let proof_a = tree.prove(&base_addr);
            let proof_b = tree.prove(&neighbor);
            prop_assert!(proof_a.verify(), "proof for base key must verify");
            prop_assert!(proof_b.verify(), "proof for bit-adjacent key must verify");
        }

        /// Deleting one key from a multi-key tree preserves proofs for remaining keys.
        #[test]
        fn delete_preserves_other_proofs(
            entries in prop::collection::vec((arb_address(), arb_h256()), 3..15),
            delete_idx in any::<prop::sample::Index>(),
        ) {
            let mut tree = SparseMerkleTree::new();
            let mut unique_entries = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for (addr, val) in &entries {
                if seen.insert(*addr) {
                    unique_entries.push((*addr, *val));
                    tree.update(*addr, *val);
                }
            }
            prop_assume!(unique_entries.len() >= 3);
            let del_idx = delete_idx.index(unique_entries.len());
            let del_addr = unique_entries[del_idx].0;
            tree.delete(&del_addr);

            for (i, (addr, _val)) in unique_entries.iter().enumerate() {
                if i == del_idx {
                    let proof = tree.prove(addr);
                    prop_assert!(proof.verify());
                    prop_assert_eq!(proof.value_hash, None, "deleted key must have exclusion proof");
                } else {
                    let proof = tree.prove(addr);
                    prop_assert!(proof.verify(), "surviving key at index {} must still have valid proof", i);
                    prop_assert!(proof.value_hash.is_some());
                }
            }
        }
    }
}
