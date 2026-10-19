use ed25519_dalek::{Signer, SigningKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

use crate::error::{KesError, Result};
use crate::signature::{KesSignature, KesVerificationKey};

/// Key-Evolving Signature (KES) scheme using a binary tree of Ed25519 keypairs.
///
/// Based on the MMM (Malkin-Micciancio-Miner) sum composition construction
/// used in Cardano's Praos protocol.
///
/// The key tree has `depth` levels, supporting `2^depth` time periods.
/// At each period, only one leaf keypair is active. When the key evolves
/// to the next period, the old leaf's secret key is securely erased,
/// providing forward secrecy: compromise of the current key cannot forge
/// signatures for past periods.
///
/// Signature structure:
/// - Ed25519 signature from the active leaf keypair (64 bytes)
/// - Authentication path: sibling hashes from leaf to root (depth * 32 bytes)
/// - Active leaf's public key (32 bytes)
///
/// Verification:
/// 1. Verify the Ed25519 signature against the leaf public key
/// 2. Reconstruct the Merkle root from the leaf public key hash + auth path
/// 3. Compare against the stored root (verification key)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KesKey {
    /// All leaf signing keys. Evolved (erased) keys are set to None.
    leaves: Vec<Option<[u8; 32]>>,
    /// All leaf public keys (always available for auth path computation).
    leaf_pubkeys: Vec<[u8; 32]>,
    /// Merkle root of the leaf public key hashes.
    root: [u8; 32],
    /// Current period (monotonically increasing).
    current_period: u32,
    /// Tree depth (supports 2^depth periods).
    depth: u32,
}

impl KesKey {
    /// Generate a new KES key supporting `max_periods` time periods.
    /// `max_periods` is rounded up to the next power of 2.
    #[must_use]
    pub fn generate(max_periods: u32) -> Self {
        let max_periods = max_periods.max(2);
        let depth = (max_periods as f64).log2().ceil() as u32;
        let num_leaves = 1u32 << depth;

        let mut leaves = Vec::with_capacity(num_leaves as usize);
        let mut leaf_pubkeys = Vec::with_capacity(num_leaves as usize);

        for _ in 0..num_leaves {
            let signing_key = SigningKey::generate(&mut OsRng);
            let verifying_key = signing_key.verifying_key();
            leaves.push(Some(signing_key.to_bytes()));
            leaf_pubkeys.push(verifying_key.to_bytes());
        }

        let root = compute_merkle_root(&leaf_pubkeys);

        KesKey {
            leaves,
            leaf_pubkeys,
            root,
            current_period: 0,
            depth,
        }
    }

    /// Create a KES key from an explicit seed (deterministic).
    #[must_use]
    pub fn from_seed(seed: [u8; 32], max_periods: u32) -> Self {
        let max_periods = max_periods.max(2);
        let depth = (max_periods as f64).log2().ceil() as u32;
        let num_leaves = 1u32 << depth;

        let mut leaves = Vec::with_capacity(num_leaves as usize);
        let mut leaf_pubkeys = Vec::with_capacity(num_leaves as usize);

        for i in 0..num_leaves {
            // Derive each leaf's secret deterministically from seed
            let mut hasher = Sha256::new();
            hasher.update(seed);
            hasher.update(b"kes-leaf");
            hasher.update(i.to_le_bytes());
            let leaf_seed: [u8; 32] = hasher.finalize().into();

            let signing_key = SigningKey::from_bytes(&leaf_seed);
            let verifying_key = signing_key.verifying_key();
            leaves.push(Some(leaf_seed));
            leaf_pubkeys.push(verifying_key.to_bytes());
        }

        let root = compute_merkle_root(&leaf_pubkeys);

        KesKey {
            leaves,
            leaf_pubkeys,
            root,
            current_period: 0,
            depth,
        }
    }

    /// Maximum number of supported periods.
    #[inline]
    #[must_use]
    pub fn max_periods(&self) -> u32 {
        1u32 << self.depth
    }

    /// Current evolved period.
    #[inline]
    #[must_use]
    pub fn current_period(&self) -> u32 {
        self.current_period
    }

    /// Public verification key (Merkle root + metadata).
    #[must_use]
    pub fn verification_key(&self) -> KesVerificationKey {
        KesVerificationKey::new(self.root, self.max_periods())
    }

    /// Sign a message at the given period.
    ///
    /// The period must be >= current_period (forward only).
    /// Signing at a period automatically evolves the key to that period,
    /// erasing all leaf keys for periods < `period`.
    pub fn sign(&mut self, period: u32, message: &[u8]) -> Result<KesSignature> {
        if period >= self.max_periods() {
            return Err(KesError::PeriodOutOfRange {
                requested: period,
                max_periods: self.max_periods(),
            });
        }

        if period < self.current_period {
            return Err(KesError::PeriodRegression {
                current: self.current_period,
                requested: period,
            });
        }

        // Evolve: erase all keys for periods before `period`
        self.evolve_to(period);

        // Get the active leaf's signing key
        let leaf_secret = self.leaves[period as usize]
            .as_ref()
            .ok_or(KesError::KeyErased { period })?;

        let signing_key = SigningKey::from_bytes(leaf_secret);
        let ed_signature = signing_key.sign(message);

        // Build authentication path (sibling hashes from leaf to root)
        let auth_path = compute_auth_path(&self.leaf_pubkeys, period as usize);

        Ok(KesSignature {
            period,
            signature: ed_signature.to_bytes().to_vec(),
            leaf_pubkey: self.leaf_pubkeys[period as usize],
            auth_path,
        })
    }

    /// Evolve the key to a target period, securely erasing past keys.
    fn evolve_to(&mut self, target_period: u32) {
        for i in self.current_period..target_period {
            if let Some(ref mut key_bytes) = self.leaves[i as usize] {
                key_bytes.zeroize();
            }
            self.leaves[i as usize] = None;
        }
        self.current_period = target_period;
    }
}

impl Drop for KesKey {
    fn drop(&mut self) {
        // Zeroize all remaining secret key material
        for key_bytes in self.leaves.iter_mut().flatten() {
            key_bytes.zeroize();
        }
    }
}

/// Compute the Merkle root from leaf public keys.
fn compute_merkle_root(leaf_pubkeys: &[[u8; 32]]) -> [u8; 32] {
    // Hash each leaf public key
    let mut current_level: Vec<[u8; 32]> = leaf_pubkeys
        .iter()
        .map(|pk| {
            let mut h = Sha256::new();
            h.update([0x00]);
            h.update(pk);
            h.finalize().into()
        })
        .collect();

    // Build tree bottom-up
    while current_level.len() > 1 {
        let mut next_level = Vec::with_capacity(current_level.len() / 2);
        for pair in current_level.chunks(2) {
            let mut h = Sha256::new();
            h.update([0x01]);
            h.update(pair[0]);
            if pair.len() > 1 {
                h.update(pair[1]);
            } else {
                h.update(pair[0]); // duplicate if odd
            }
            next_level.push(h.finalize().into());
        }
        current_level = next_level;
    }

    current_level[0]
}

/// Compute the authentication path (sibling hashes) for a given leaf index.
fn compute_auth_path(leaf_pubkeys: &[[u8; 32]], leaf_index: usize) -> Vec<[u8; 32]> {
    // Hash each leaf public key
    let mut current_level: Vec<[u8; 32]> = leaf_pubkeys
        .iter()
        .map(|pk| {
            let mut h = Sha256::new();
            h.update([0x00]);
            h.update(pk);
            h.finalize().into()
        })
        .collect();

    let mut path = Vec::new();
    let mut idx = leaf_index;

    while current_level.len() > 1 {
        // Sibling index
        let sibling_idx = idx ^ 1;
        if sibling_idx < current_level.len() {
            path.push(current_level[sibling_idx]);
        } else {
            path.push(current_level[idx]); // duplicate if no sibling
        }

        // Build next level
        let mut next_level = Vec::with_capacity(current_level.len() / 2);
        for pair in current_level.chunks(2) {
            let mut h = Sha256::new();
            h.update([0x01]);
            h.update(pair[0]);
            if pair.len() > 1 {
                h.update(pair[1]);
            } else {
                h.update(pair[0]);
            }
            next_level.push(h.finalize().into());
        }

        current_level = next_level;
        idx /= 2;
    }

    path
}

/// Verify a Merkle authentication path from a leaf to the expected root.
pub(crate) fn verify_auth_path(
    leaf_pubkey: &[u8; 32],
    leaf_index: usize,
    auth_path: &[[u8; 32]],
    expected_root: &[u8; 32],
) -> bool {
    if auth_path.is_empty() {
        return false;
    }
    let max_leaves = 1usize << auth_path.len();
    if leaf_index >= max_leaves {
        return false;
    }

    // Hash the leaf public key
    let mut current_hash: [u8; 32] = {
        let mut h = Sha256::new();
        h.update([0x00]);
        h.update(leaf_pubkey);
        h.finalize().into()
    };

    let mut idx = leaf_index;
    for sibling in auth_path {
        let mut h = Sha256::new();
        h.update([0x01]);
        if idx % 2 == 0 {
            h.update(current_hash);
            h.update(sibling);
        } else {
            h.update(sibling);
            h.update(current_hash);
        }
        current_hash = h.finalize().into();
        idx /= 2;
    }

    bool::from(current_hash.ct_eq(expected_root))
}
