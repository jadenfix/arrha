//! Post-quantum cryptography boundaries for Aether agents.
//!
//! This crate keeps ML-DSA and ML-KEM available to SDKs, agent identity,
//! transport setup, and long-lived credentials without adding post-quantum
//! verification to the consensus hot path.

use fips203::ml_kem_768;
use fips203::traits::{Decaps, Encaps, KeyGen as KemKeyGen, SerDes as KemSerDes};
use fips204::traits::{SerDes as SignatureSerDes, Signer, Verifier};
use fips204::{ml_dsa_65, ml_dsa_87};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const ML_DSA_65_PUBLIC_KEY_LEN: usize = ml_dsa_65::PK_LEN;
pub const ML_DSA_65_PRIVATE_KEY_LEN: usize = ml_dsa_65::SK_LEN;
pub const ML_DSA_65_SIGNATURE_LEN: usize = ml_dsa_65::SIG_LEN;
pub const ML_DSA_87_PUBLIC_KEY_LEN: usize = ml_dsa_87::PK_LEN;
pub const ML_DSA_87_PRIVATE_KEY_LEN: usize = ml_dsa_87::SK_LEN;
pub const ML_DSA_87_SIGNATURE_LEN: usize = ml_dsa_87::SIG_LEN;
pub const ML_KEM_768_PUBLIC_KEY_LEN: usize = ml_kem_768::EK_LEN;
pub const ML_KEM_768_PRIVATE_KEY_LEN: usize = ml_kem_768::DK_LEN;
pub const ML_KEM_768_CIPHERTEXT_LEN: usize = ml_kem_768::CT_LEN;
pub const ML_KEM_SHARED_SECRET_LEN: usize = fips203::SSK_LEN;
pub const MAX_ML_DSA_CONTEXT_LEN: usize = 255;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PqCryptoError {
    #[error("unsupported post-quantum algorithm")]
    UnsupportedAlgorithm,
    #[error("ML-DSA context must be 255 bytes or fewer")]
    ContextTooLong,
    #[error("invalid ML-DSA-65 public key")]
    InvalidMlDsa65PublicKey,
    #[error("invalid ML-DSA-65 private key")]
    InvalidMlDsa65PrivateKey,
    #[error("invalid ML-DSA-65 signature")]
    InvalidMlDsa65Signature,
    #[error("ML-DSA-65 signature verification failed")]
    MlDsa65VerificationFailed,
    #[error("invalid ML-DSA-87 public key")]
    InvalidMlDsa87PublicKey,
    #[error("invalid ML-DSA-87 private key")]
    InvalidMlDsa87PrivateKey,
    #[error("invalid ML-DSA-87 signature")]
    InvalidMlDsa87Signature,
    #[error("ML-DSA-87 signature verification failed")]
    MlDsa87VerificationFailed,
    #[error("invalid ML-KEM-768 encapsulation key")]
    InvalidMlKem768PublicKey,
    #[error("invalid ML-KEM-768 decapsulation key")]
    InvalidMlKem768PrivateKey,
    #[error("invalid ML-KEM-768 ciphertext")]
    InvalidMlKem768Ciphertext,
    #[error("post-quantum backend failed: {0}")]
    Backend(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PqSignatureAlgorithm {
    MlDsa65,
    MlDsa87,
}

impl PqSignatureAlgorithm {
    #[must_use]
    pub const fn context_label(self) -> &'static str {
        match self {
            PqSignatureAlgorithm::MlDsa65 => "ml-dsa-65",
            PqSignatureAlgorithm::MlDsa87 => "ml-dsa-87",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PqKemAlgorithm {
    MlKem768,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MlDsa65Keypair {
    pub public_key: Vec<u8>,
    pub private_key: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MlDsa87Keypair {
    pub public_key: Vec<u8>,
    pub private_key: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PqSignatureEnvelope {
    pub alg: PqSignatureAlgorithm,
    pub context: Vec<u8>,
    pub public_key: Vec<u8>,
    pub signature: Vec<u8>,
}

impl PqSignatureEnvelope {
    pub fn validate(&self) -> Result<(), PqCryptoError> {
        ensure_context(&self.context)?;
        match self.alg {
            PqSignatureAlgorithm::MlDsa65 => {
                expect_len(
                    &self.public_key,
                    ML_DSA_65_PUBLIC_KEY_LEN,
                    PqCryptoError::InvalidMlDsa65PublicKey,
                )?;
                expect_len(
                    &self.signature,
                    ML_DSA_65_SIGNATURE_LEN,
                    PqCryptoError::InvalidMlDsa65Signature,
                )?;
                Ok(())
            }
            PqSignatureAlgorithm::MlDsa87 => {
                expect_len(
                    &self.public_key,
                    ML_DSA_87_PUBLIC_KEY_LEN,
                    PqCryptoError::InvalidMlDsa87PublicKey,
                )?;
                expect_len(
                    &self.signature,
                    ML_DSA_87_SIGNATURE_LEN,
                    PqCryptoError::InvalidMlDsa87Signature,
                )?;
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HybridIdentityProof {
    pub classical_alg: String,
    pub classical_public_key: Vec<u8>,
    pub classical_signature: Vec<u8>,
    pub pq_signature: PqSignatureEnvelope,
}

impl HybridIdentityProof {
    pub fn validate_pq(&self) -> Result<(), PqCryptoError> {
        self.pq_signature.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MlKem768Keypair {
    pub public_key: Vec<u8>,
    pub private_key: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MlKem768Encapsulation {
    pub ciphertext: Vec<u8>,
    pub shared_secret: [u8; ML_KEM_SHARED_SECRET_LEN],
}

#[must_use]
pub fn pq_signature_context(domain: &str, chain_id: u64) -> Vec<u8> {
    pq_signature_context_for_alg(domain, chain_id, PqSignatureAlgorithm::MlDsa65)
}

#[must_use]
pub fn pq_signature_context_for_alg(
    domain: &str,
    chain_id: u64,
    alg: PqSignatureAlgorithm,
) -> Vec<u8> {
    format!("aether/{domain}/chain/{chain_id}/{}", alg.context_label()).into_bytes()
}

pub fn generate_ml_dsa65_keypair() -> Result<MlDsa65Keypair, PqCryptoError> {
    let (public_key, private_key) = ml_dsa_65::try_keygen().map_err(PqCryptoError::Backend)?;
    Ok(MlDsa65Keypair {
        public_key: public_key.into_bytes().to_vec(),
        private_key: private_key.into_bytes().to_vec(),
    })
}

pub fn sign_ml_dsa65(
    private_key: &[u8],
    context: &[u8],
    message: &[u8],
) -> Result<PqSignatureEnvelope, PqCryptoError> {
    ensure_context(context)?;
    let private_key = ml_dsa_65::PrivateKey::try_from_bytes(to_array::<ML_DSA_65_PRIVATE_KEY_LEN>(
        private_key,
        PqCryptoError::InvalidMlDsa65PrivateKey,
    )?)
    .map_err(|_| PqCryptoError::InvalidMlDsa65PrivateKey)?;
    let public_key = private_key.get_public_key();
    let signature = private_key
        .try_sign(message, context)
        .map_err(PqCryptoError::Backend)?;

    Ok(PqSignatureEnvelope {
        alg: PqSignatureAlgorithm::MlDsa65,
        context: context.to_vec(),
        public_key: public_key.into_bytes().to_vec(),
        signature: signature.to_vec(),
    })
}

pub fn generate_ml_dsa87_keypair() -> Result<MlDsa87Keypair, PqCryptoError> {
    let (public_key, private_key) = ml_dsa_87::try_keygen().map_err(PqCryptoError::Backend)?;
    Ok(MlDsa87Keypair {
        public_key: public_key.into_bytes().to_vec(),
        private_key: private_key.into_bytes().to_vec(),
    })
}

pub fn sign_ml_dsa87(
    private_key: &[u8],
    context: &[u8],
    message: &[u8],
) -> Result<PqSignatureEnvelope, PqCryptoError> {
    ensure_context(context)?;
    let private_key = ml_dsa_87::PrivateKey::try_from_bytes(to_array::<ML_DSA_87_PRIVATE_KEY_LEN>(
        private_key,
        PqCryptoError::InvalidMlDsa87PrivateKey,
    )?)
    .map_err(|_| PqCryptoError::InvalidMlDsa87PrivateKey)?;
    let public_key = private_key.get_public_key();
    let signature = private_key
        .try_sign(message, context)
        .map_err(PqCryptoError::Backend)?;

    Ok(PqSignatureEnvelope {
        alg: PqSignatureAlgorithm::MlDsa87,
        context: context.to_vec(),
        public_key: public_key.into_bytes().to_vec(),
        signature: signature.to_vec(),
    })
}

#[must_use = "discarding PQ signature verification results is a security bug"]
pub fn verify_ml_dsa65(
    public_key: &[u8],
    context: &[u8],
    message: &[u8],
    signature: &[u8],
) -> Result<(), PqCryptoError> {
    ensure_context(context)?;
    let public_key = ml_dsa_65::PublicKey::try_from_bytes(to_array::<ML_DSA_65_PUBLIC_KEY_LEN>(
        public_key,
        PqCryptoError::InvalidMlDsa65PublicKey,
    )?)
    .map_err(|_| PqCryptoError::InvalidMlDsa65PublicKey)?;
    let signature =
        to_array::<ML_DSA_65_SIGNATURE_LEN>(signature, PqCryptoError::InvalidMlDsa65Signature)?;
    if public_key.verify(message, &signature, context) {
        Ok(())
    } else {
        Err(PqCryptoError::MlDsa65VerificationFailed)
    }
}

#[must_use = "discarding PQ signature verification results is a security bug"]
pub fn verify_ml_dsa87(
    public_key: &[u8],
    context: &[u8],
    message: &[u8],
    signature: &[u8],
) -> Result<(), PqCryptoError> {
    ensure_context(context)?;
    let public_key = ml_dsa_87::PublicKey::try_from_bytes(to_array::<ML_DSA_87_PUBLIC_KEY_LEN>(
        public_key,
        PqCryptoError::InvalidMlDsa87PublicKey,
    )?)
    .map_err(|_| PqCryptoError::InvalidMlDsa87PublicKey)?;
    let signature =
        to_array::<ML_DSA_87_SIGNATURE_LEN>(signature, PqCryptoError::InvalidMlDsa87Signature)?;
    if public_key.verify(message, &signature, context) {
        Ok(())
    } else {
        Err(PqCryptoError::MlDsa87VerificationFailed)
    }
}

#[must_use = "discarding PQ signature verification results is a security bug"]
pub fn verify_pq_signature(
    envelope: &PqSignatureEnvelope,
    message: &[u8],
) -> Result<(), PqCryptoError> {
    envelope.validate()?;
    match envelope.alg {
        PqSignatureAlgorithm::MlDsa65 => verify_ml_dsa65(
            &envelope.public_key,
            &envelope.context,
            message,
            &envelope.signature,
        ),
        PqSignatureAlgorithm::MlDsa87 => verify_ml_dsa87(
            &envelope.public_key,
            &envelope.context,
            message,
            &envelope.signature,
        ),
    }
}

pub fn generate_ml_kem768_keypair() -> Result<MlKem768Keypair, PqCryptoError> {
    let (public_key, private_key) = ml_kem_768::KG::try_keygen().map_err(PqCryptoError::Backend)?;
    Ok(MlKem768Keypair {
        public_key: public_key.into_bytes().to_vec(),
        private_key: private_key.into_bytes().to_vec(),
    })
}

pub fn encapsulate_ml_kem768(public_key: &[u8]) -> Result<MlKem768Encapsulation, PqCryptoError> {
    let public_key = ml_kem_768::EncapsKey::try_from_bytes(to_array::<ML_KEM_768_PUBLIC_KEY_LEN>(
        public_key,
        PqCryptoError::InvalidMlKem768PublicKey,
    )?)
    .map_err(|_| PqCryptoError::InvalidMlKem768PublicKey)?;
    let (shared_secret, ciphertext) = public_key.try_encaps().map_err(PqCryptoError::Backend)?;
    Ok(MlKem768Encapsulation {
        ciphertext: ciphertext.into_bytes().to_vec(),
        shared_secret: shared_secret.into_bytes(),
    })
}

pub fn decapsulate_ml_kem768(
    private_key: &[u8],
    ciphertext: &[u8],
) -> Result<[u8; ML_KEM_SHARED_SECRET_LEN], PqCryptoError> {
    let private_key =
        ml_kem_768::DecapsKey::try_from_bytes(to_array::<ML_KEM_768_PRIVATE_KEY_LEN>(
            private_key,
            PqCryptoError::InvalidMlKem768PrivateKey,
        )?)
        .map_err(|_| PqCryptoError::InvalidMlKem768PrivateKey)?;
    let ciphertext = ml_kem_768::CipherText::try_from_bytes(to_array::<ML_KEM_768_CIPHERTEXT_LEN>(
        ciphertext,
        PqCryptoError::InvalidMlKem768Ciphertext,
    )?)
    .map_err(|_| PqCryptoError::InvalidMlKem768Ciphertext)?;
    private_key
        .try_decaps(&ciphertext)
        .map(KemSerDes::into_bytes)
        .map_err(PqCryptoError::Backend)
}

fn ensure_context(context: &[u8]) -> Result<(), PqCryptoError> {
    if context.len() > MAX_ML_DSA_CONTEXT_LEN {
        return Err(PqCryptoError::ContextTooLong);
    }
    Ok(())
}

fn expect_len(actual: &[u8], expected: usize, err: PqCryptoError) -> Result<(), PqCryptoError> {
    if actual.len() == expected {
        Ok(())
    } else {
        Err(err)
    }
}

fn to_array<const N: usize>(bytes: &[u8], err: PqCryptoError) -> Result<[u8; N], PqCryptoError> {
    bytes.try_into().map_err(|_| err)
}
