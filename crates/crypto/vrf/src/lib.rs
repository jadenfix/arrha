pub mod ecvrf;

pub use ecvrf::{check_leader_eligibility_integer, verify_proof, VrfKeypair, VrfProof};

#[allow(deprecated)]
pub use ecvrf::{check_leader_eligibility, output_to_value};

use anyhow::Result;

/// VRF signing: produces proofs binding a pseudorandom output to a secret key
/// and an input message. Extracted as a trait so consensus can be tested with
/// deterministic mock implementations.
pub trait VrfSigner: Send + Sync {
    fn public_key_bytes(&self) -> [u8; 32];
    fn prove(&self, alpha: &[u8]) -> VrfProof;
}

/// VRF verification: validates that a proof was correctly produced by the holder
/// of the secret key corresponding to the given public key.
pub trait VrfVerifier: Send + Sync {
    fn verify(&self, public_key: &[u8; 32], alpha: &[u8], proof: &VrfProof) -> Result<bool>;
}

impl VrfSigner for VrfKeypair {
    fn public_key_bytes(&self) -> [u8; 32] {
        *self.public_key()
    }

    fn prove(&self, alpha: &[u8]) -> VrfProof {
        VrfKeypair::prove(self, alpha)
    }
}

/// ECVRF-EDWARDS25519-SHA512-ELL2 verifier (RFC 9381).
pub struct EcVrfVerifier;

impl VrfVerifier for EcVrfVerifier {
    fn verify(&self, public_key: &[u8; 32], alpha: &[u8], proof: &VrfProof) -> Result<bool> {
        verify_proof(public_key, alpha, proof)
    }
}

pub mod mock {
    use super::*;
    use sha2::{Digest, Sha256};

    /// Deterministic mock VRF for testing. Output = SHA-256(public_key || alpha).
    /// All proofs verify. Useful for writing consensus tests with predictable
    /// leader election outcomes.
    #[derive(Clone, Debug)]
    pub struct MockVrfSigner {
        public_key: [u8; 32],
    }

    impl MockVrfSigner {
        #[inline]
        #[must_use]
        pub fn new(public_key: [u8; 32]) -> Self {
            Self { public_key }
        }

        #[inline]
        #[must_use]
        pub fn from_index(index: u8) -> Self {
            let mut pk = [0u8; 32];
            pk[0] = index;
            Self { public_key: pk }
        }
    }

    impl VrfSigner for MockVrfSigner {
        fn public_key_bytes(&self) -> [u8; 32] {
            self.public_key
        }

        fn prove(&self, alpha: &[u8]) -> VrfProof {
            let mut hasher = Sha256::new();
            hasher.update(self.public_key);
            hasher.update(alpha);
            let hash = hasher.finalize();
            let mut output = [0u8; 32];
            output.copy_from_slice(&hash);

            VrfProof {
                proof: vec![0u8; 80],
                output,
            }
        }
    }

    /// Mock verifier that accepts all proofs (for testing consensus logic
    /// without real cryptographic verification).
    pub struct MockVrfVerifier;

    impl VrfVerifier for MockVrfVerifier {
        fn verify(&self, _public_key: &[u8; 32], _alpha: &[u8], _proof: &VrfProof) -> Result<bool> {
            Ok(true)
        }
    }

    /// Mock verifier that rejects all proofs.
    pub struct RejectAllVrfVerifier;

    impl VrfVerifier for RejectAllVrfVerifier {
        fn verify(&self, _public_key: &[u8; 32], _alpha: &[u8], _proof: &VrfProof) -> Result<bool> {
            Ok(false)
        }
    }
}
