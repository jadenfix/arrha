use ed25519_dalek::{Signature as DalekSignature, Signer, SigningKey, Verifier, VerifyingKey};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Ed25519Error {
    #[error("invalid signature")]
    Signature,
    #[error("invalid public key")]
    PublicKey,
    #[error("invalid secret key")]
    SecretKey,
}

pub struct Keypair {
    signing_key: SigningKey,
}

impl Keypair {
    #[must_use]
    pub fn generate() -> Self {
        let mut rng = rand::thread_rng();
        let signing_key = SigningKey::generate(&mut rng);
        Keypair { signing_key }
    }

    #[must_use = "constructing a Keypair without binding it is a no-op"]
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Ed25519Error> {
        if bytes.len() != 32 {
            return Err(Ed25519Error::SecretKey);
        }
        let mut key_bytes = [0u8; 32];
        key_bytes.copy_from_slice(bytes);
        let signing_key = SigningKey::from_bytes(&key_bytes);
        Ok(Keypair { signing_key })
    }

    #[inline]
    #[must_use]
    pub fn public_key(&self) -> Vec<u8> {
        self.signing_key.verifying_key().to_bytes().to_vec()
    }

    #[inline]
    #[must_use]
    pub fn secret_key(&self) -> Vec<u8> {
        self.signing_key.to_bytes().to_vec()
    }

    #[must_use]
    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.signing_key.sign(message).to_bytes().to_vec()
    }
}

#[must_use = "discarding a signature verification result is a security bug"]
pub fn verify(public_key: &[u8], message: &[u8], signature: &[u8]) -> Result<(), Ed25519Error> {
    if public_key.len() != 32 {
        return Err(Ed25519Error::PublicKey);
    }
    if signature.len() != 64 {
        return Err(Ed25519Error::Signature);
    }

    let mut pk_bytes = [0u8; 32];
    pk_bytes.copy_from_slice(public_key);
    let verifying_key = VerifyingKey::from_bytes(&pk_bytes).map_err(|_| Ed25519Error::PublicKey)?;

    let mut sig_bytes = [0u8; 64];
    sig_bytes.copy_from_slice(signature);
    let signature = DalekSignature::from_bytes(&sig_bytes);

    verifying_key
        .verify(message, &signature)
        .map_err(|_| Ed25519Error::Signature)?;

    Ok(())
}

/// Batch signature verification - optimized for verifying multiple signatures at once
///
/// Phase 4.2: CPU-optimized batch verification with hooks for future GPU acceleration
/// Target: ≥ 300k sig/s per node (Phase 4 acceptance criteria)
///
/// Currently implements parallel CPU verification. Future enhancements:
/// - GPU batch verification via CUDA/OpenCL for 300k+/s throughput
/// - SIMD optimizations for vectorized operations
#[must_use = "discarding a batch verification result is a security bug"]
pub fn verify_batch(
    verifications: &[(Vec<u8>, Vec<u8>, Vec<u8>)], // (public_key, message, signature) tuples
) -> Result<Vec<bool>, Ed25519Error> {
    use rayon::prelude::*;

    if verifications.is_empty() {
        return Ok(Vec::new());
    }

    // Prepare inputs for Dalek batch verification
    let mut verifying_keys = Vec::with_capacity(verifications.len());
    let mut signatures = Vec::with_capacity(verifications.len());
    let mut message_refs = Vec::with_capacity(verifications.len());

    for (pk, msg, sig) in verifications {
        if pk.len() != 32 {
            return Err(Ed25519Error::PublicKey);
        }
        if sig.len() != 64 {
            return Err(Ed25519Error::Signature);
        }

        let mut pk_bytes = [0u8; 32];
        pk_bytes.copy_from_slice(pk);
        let vk = VerifyingKey::from_bytes(&pk_bytes).map_err(|_| Ed25519Error::PublicKey)?;

        let mut sig_bytes = [0u8; 64];
        sig_bytes.copy_from_slice(sig);
        let dalek_sig = DalekSignature::from_bytes(&sig_bytes);

        verifying_keys.push(vk);
        signatures.push(dalek_sig);
        message_refs.push(msg.as_slice());
    }

    let results: Vec<bool> = signatures
        .par_iter()
        .enumerate()
        .map(|(idx, signature)| {
            verifying_keys[idx]
                .verify(message_refs[idx], signature)
                .is_ok()
        })
        .collect();

    Ok(results)
}

/// Batch verification returning only count of successful verifications
/// Optimized for consensus vote aggregation where individual failures don't matter
pub fn verify_batch_count(
    verifications: &[(Vec<u8>, Vec<u8>, Vec<u8>)],
) -> Result<usize, Ed25519Error> {
    let results = verify_batch(verifications)?;
    Ok(results.into_iter().filter(|v| *v).count())
}
