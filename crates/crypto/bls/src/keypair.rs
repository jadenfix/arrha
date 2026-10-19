use anyhow::{anyhow, Result};
use blst::min_pk::{
    PublicKey as BlstPublicKey, SecretKey as BlstSecretKey, Signature as BlstSignature,
};
use blst::BLST_ERROR;

const DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_NUL_";
/// Domain separation tag for proof-of-possession (prevents rogue key attacks).
const DST_POP: &[u8] = b"BLS_POP_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";

/// BLS12-381 Keypair for signing and verification
///
/// Uses BLS12-381 curve for:
/// - Signature aggregation (combine 1000s of signatures into one)
/// - Public key aggregation
/// - Single pairing verification for all aggregated signatures
///
/// Key sizes:
/// - Secret key: 32 bytes
/// - Public key: 48 bytes (G1 point compressed)
/// - Signature: 96 bytes (G2 point compressed)

#[derive(Clone)]
pub struct BlsKeypair {
    secret: BlstSecretKey,
    public: BlstPublicKey,
}

impl BlsKeypair {
    /// Generate a new random BLS keypair
    #[must_use]
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut rng = rand::thread_rng();

        // Generate 32-byte secret key
        let mut ikm = [0u8; 32];
        rng.fill_bytes(&mut ikm);

        let secret =
            BlstSecretKey::key_gen(&ikm, &[]).expect("random IKM always valid for key generation");
        let public = secret.sk_to_pk();

        BlsKeypair { secret, public }
    }

    /// Create keypair from secret key
    #[must_use = "constructing a keypair without binding it is a no-op"]
    pub fn from_secret(secret: Vec<u8>) -> Result<Self> {
        if secret.len() != 32 {
            anyhow::bail!("BLS secret key must be 32 bytes");
        }

        let secret = BlstSecretKey::from_bytes(&secret)
            .map_err(|e| anyhow!("invalid secret key bytes: {:?}", e))?;
        let public = secret.sk_to_pk();

        Ok(BlsKeypair { secret, public })
    }

    /// Sign a message
    /// Returns 96-byte BLS signature
    #[must_use]
    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.secret.sign(message, DST, &[]).to_bytes().to_vec()
    }

    /// Get public key (compressed 48-byte form)
    #[inline]
    #[must_use]
    pub fn public_key(&self) -> Vec<u8> {
        self.public.to_bytes().to_vec()
    }

    /// Get secret key bytes (32-byte scalar)
    #[inline]
    #[must_use]
    pub fn secret_key(&self) -> Vec<u8> {
        self.secret.to_bytes().to_vec()
    }

    /// Generate a proof-of-possession: sign the public key bytes with a distinct DST.
    /// PoP proves the holder knows the secret key corresponding to their public key,
    /// preventing rogue key attacks in BLS aggregate signatures.
    #[must_use]
    pub fn proof_of_possession(&self) -> Vec<u8> {
        let pk_bytes = self.public.to_bytes();
        self.secret
            .sign(&pk_bytes, DST_POP, &[])
            .to_bytes()
            .to_vec()
    }
}

/// Verify a proof-of-possession for a BLS public key.
/// Returns true if the PoP is valid (the key holder proved knowledge of the secret key).
#[must_use = "discarding a PoP verification result is a security bug"]
pub fn verify_pop(public_key: &[u8], pop_signature: &[u8]) -> Result<bool> {
    if public_key.len() != 48 {
        anyhow::bail!("BLS public key must be 48 bytes for PoP verification");
    }
    if pop_signature.len() != 96 {
        anyhow::bail!("PoP signature must be 96 bytes");
    }

    let pk = BlstPublicKey::from_bytes(public_key)
        .map_err(|e| anyhow!("invalid public key: {:?}", e))?;
    let sig = BlstSignature::from_bytes(pop_signature)
        .map_err(|e| anyhow!("invalid PoP signature: {:?}", e))?;

    // Verify: the PoP is a signature over the public key bytes using DST_POP
    Ok(sig.verify(true, public_key, DST_POP, &[], &pk, true) == BLST_ERROR::BLST_SUCCESS)
}

/// Verify a single BLS signature
#[must_use = "discarding a signature verification result is a security bug"]
pub fn verify(public_key: &[u8], _message: &[u8], signature: &[u8]) -> Result<bool> {
    if public_key.len() != 48 {
        anyhow::bail!("BLS public key must be 48 bytes");
    }

    if signature.len() != 96 {
        anyhow::bail!("BLS signature must be 96 bytes");
    }

    let pk = BlstPublicKey::from_bytes(public_key)
        .map_err(|e| anyhow!("invalid public key: {:?}", e))?;
    let sig = BlstSignature::from_bytes(signature)
        .map_err(|e| anyhow!("invalid signature bytes: {:?}", e))?;

    Ok(sig.verify(true, _message, DST, &[], &pk, true) == BLST_ERROR::BLST_SUCCESS)
}
