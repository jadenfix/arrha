//! Threshold signature verification for agent guardian approvals.
//!
//! This crate intentionally wraps the upstream Zcash FROST implementation
//! instead of implementing threshold Schnorr signing locally.

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ThresholdError {
    #[error("unsupported threshold algorithm")]
    UnsupportedAlgorithm,
    #[error("invalid FROST verifying key")]
    InvalidVerifyingKey,
    #[error("invalid FROST signature")]
    InvalidSignatureEncoding,
    #[error("FROST signature verification failed")]
    VerificationFailed,
}

#[must_use = "discarding threshold verification results is a security bug"]
pub fn verify_frost_ristretto255(
    verifying_key: &[u8],
    message: &[u8],
    signature: &[u8],
) -> Result<(), ThresholdError> {
    let verifying_key: [u8; 32] = verifying_key
        .try_into()
        .map_err(|_| ThresholdError::InvalidVerifyingKey)?;
    let signature: [u8; 64] = signature
        .try_into()
        .map_err(|_| ThresholdError::InvalidSignatureEncoding)?;
    let verifying_key = frost_ristretto255::VerifyingKey::deserialize(&verifying_key)
        .map_err(|_| ThresholdError::InvalidVerifyingKey)?;
    let signature = frost_ristretto255::Signature::deserialize(&signature)
        .map_err(|_| ThresholdError::InvalidSignatureEncoding)?;
    verifying_key
        .verify(message, &signature)
        .map_err(|_| ThresholdError::VerificationFailed)
}
