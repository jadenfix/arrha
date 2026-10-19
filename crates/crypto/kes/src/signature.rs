use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::evolution::verify_auth_path;

/// A KES signature containing the Ed25519 signature from the active leaf,
/// the leaf's public key, and the Merkle authentication path to the root.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct KesSignature {
    /// Time period this signature was produced at.
    pub period: u32,
    /// Ed25519 signature bytes (64 bytes as Vec for serde compatibility).
    pub signature: Vec<u8>,
    /// Public key of the active leaf that produced this signature (32 bytes).
    pub leaf_pubkey: [u8; 32],
    /// Merkle authentication path from leaf to root (depth * 32 bytes).
    pub auth_path: Vec<[u8; 32]>,
}

impl KesSignature {
    /// Verify the signature against a KES verification key and message.
    ///
    /// Steps:
    /// 1. Verify the Ed25519 signature against the leaf public key
    /// 2. Verify the Merkle authentication path from leaf to root
    /// 3. Compare the reconstructed root to the verification key's root
    #[must_use = "discarding a KES verification result is a security bug"]
    pub fn verify(&self, vk: &KesVerificationKey, message: &[u8]) -> bool {
        if self.period >= vk.max_periods {
            return false;
        }

        // Validate auth path length matches expected tree depth
        let expected_depth = (vk.max_periods as f64).log2().ceil() as usize;
        if self.auth_path.len() != expected_depth {
            return false;
        }

        // Step 1: Verify Ed25519 signature
        let verifying_key = match VerifyingKey::from_bytes(&self.leaf_pubkey) {
            Ok(vk) => vk,
            Err(_) => return false,
        };
        let sig_bytes: [u8; 64] = match self.signature.as_slice().try_into() {
            Ok(b) => b,
            Err(_) => return false,
        };
        let signature = Signature::from_bytes(&sig_bytes);
        if verifying_key.verify(message, &signature).is_err() {
            return false;
        }

        // Step 2: Verify Merkle authentication path
        verify_auth_path(
            &self.leaf_pubkey,
            self.period as usize,
            &self.auth_path,
            &vk.root,
        )
    }
}

/// The public verification key for a KES scheme.
/// Contains the Merkle root of all leaf public keys.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct KesVerificationKey {
    pub(crate) root: [u8; 32],
    pub(crate) max_periods: u32,
}

impl KesVerificationKey {
    #[inline]
    #[must_use]
    pub fn new(root: [u8; 32], max_periods: u32) -> Self {
        KesVerificationKey { root, max_periods }
    }

    #[inline]
    #[must_use]
    pub fn root(&self) -> [u8; 32] {
        self.root
    }

    #[inline]
    #[must_use]
    pub fn max_periods(&self) -> u32 {
        self.max_periods
    }
}
