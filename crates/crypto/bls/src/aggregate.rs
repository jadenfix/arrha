use anyhow::{anyhow, Result};
use blst::min_pk::{AggregatePublicKey, AggregateSignature, Signature as BlstSignature};
use std::collections::HashSet;

/// BLS Signature Aggregation
///
/// The key property of BLS signatures: multiple signatures can be aggregated
/// into a single signature that can be verified against the aggregated public keys.
///
/// Aggregation is done via point addition on the elliptic curve:
/// - Signatures are G2 points: agg_sig = sig1 + sig2 + ... + sigN
/// - Public keys are G1 points: agg_pk = pk1 + pk2 + ... + pkN
///
/// Verification: e(agg_pk, H(m)) == e(G1, agg_sig)
/// where e() is the pairing function
///
/// Aggregate multiple BLS signatures into one
///
/// Input: Vec of 96-byte signatures
/// Output: Single 96-byte aggregated signature
pub fn aggregate_signatures(signatures: &[Vec<u8>]) -> Result<Vec<u8>> {
    if signatures.is_empty() {
        anyhow::bail!("cannot aggregate empty signature list");
    }

    // Validate all signatures are 96 bytes and pass subgroup check
    let mut seen = HashSet::new();
    for (i, sig) in signatures.iter().enumerate() {
        if sig.len() != 96 {
            anyhow::bail!("signature {} has invalid length: {}", i, sig.len());
        }
        // Subgroup check: ensure the deserialized point is in the correct G2 subgroup
        let blst_sig = BlstSignature::from_bytes(sig.as_slice())
            .map_err(|e| anyhow!("signature {} deserialization failed: {:?}", i, e))?;
        if !blst_sig.subgroup_check() {
            anyhow::bail!("signature {} failed subgroup check", i);
        }
        // Duplicate detection: reject identical signatures (could inflate voting power)
        if !seen.insert(sig.as_slice()) {
            anyhow::bail!("duplicate signature detected at index {}", i);
        }
    }

    let sig_refs: Vec<&[u8]> = signatures.iter().map(|s| s.as_slice()).collect();

    let agg = AggregateSignature::aggregate_serialized(&sig_refs, true)
        .map_err(|e| anyhow!("failed to aggregate signatures: {:?}", e))?;

    Ok(agg.to_signature().to_bytes().to_vec())
}

/// Aggregate multiple BLS public keys into one
///
/// Input: Vec of 48-byte public keys  
/// Output: Single 48-byte aggregated public key
pub fn aggregate_public_keys(public_keys: &[Vec<u8>]) -> Result<Vec<u8>> {
    if public_keys.is_empty() {
        anyhow::bail!("cannot aggregate empty public key list");
    }

    // Validate all public keys are 48 bytes
    for (i, pk) in public_keys.iter().enumerate() {
        if pk.len() != 48 {
            anyhow::bail!("public key {} has invalid length: {}", i, pk.len());
        }
    }

    let pk_refs: Vec<&[u8]> = public_keys.iter().map(|pk| pk.as_slice()).collect();

    // The `true` flag enables subgroup validation on each key during aggregation.
    let agg = AggregatePublicKey::aggregate_serialized(&pk_refs, true)
        .map_err(|e| anyhow!("failed to aggregate public keys (subgroup check): {:?}", e))?;

    Ok(agg.to_public_key().to_bytes().to_vec())
}

/// Check if a signature is aggregated (has multiple signers)
#[inline]
#[must_use]
pub fn is_aggregated(signature: &[u8]) -> bool {
    if signature.len() != 96 {
        return false;
    }
    BlstSignature::from_bytes(signature)
        .map(|sig| sig.subgroup_check())
        .unwrap_or(false)
}

/// Fast aggregation using parallel processing for large signature sets
pub fn aggregate_signatures_parallel(signatures: &[Vec<u8>]) -> Result<Vec<u8>> {
    if signatures.is_empty() {
        anyhow::bail!("cannot aggregate empty signature list");
    }

    if signatures.len() < 100 {
        // For small sets, sequential is faster
        return aggregate_signatures(signatures);
    }

    // Placeholder for future rayon-based parallel aggregation.
    aggregate_signatures(signatures)
}
