use anyhow::{anyhow, Result};
use blst::min_pk::{PublicKey as BlstPublicKey, Signature as BlstSignature};
use blst::BLST_ERROR;
use rayon::prelude::*;

const DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_NUL_";

/// Batch-verify N individual BLS signatures via `verify_multiple_aggregate_signatures`.
///
/// Internally uses randomized multi-pairing: each (pk, msg, sig) is multiplied
/// by a random 64-bit scalar before the miller loops are combined, so N
/// verification equations collapse into one final exponentiation.  blst
/// parallelizes the miller loops across a thread pool automatically.
///
/// Returns `Ok(true)` iff ALL signatures are valid.  If any single signature
/// is bad the whole batch fails — call individual `verify()` to identify
/// which one.
///
/// Security: random scalars prevent an attacker from crafting two invalid
/// signatures whose pairing contributions cancel (Bellare et al., 2007).
#[must_use = "discarding a batch verification result is a security bug"]
pub fn verify_batch(
    verifications: &[(&[u8], &[u8], &[u8])], // (pubkey_48, message, signature_96)
) -> Result<bool> {
    if verifications.is_empty() {
        return Ok(true);
    }

    let mut pks = Vec::with_capacity(verifications.len());
    let mut sigs = Vec::with_capacity(verifications.len());
    let mut msgs: Vec<&[u8]> = Vec::with_capacity(verifications.len());

    for (pk_bytes, msg, sig_bytes) in verifications {
        if pk_bytes.len() != 48 {
            anyhow::bail!("BLS public key must be 48 bytes");
        }
        if sig_bytes.len() != 96 {
            anyhow::bail!("BLS signature must be 96 bytes");
        }
        pks.push(
            BlstPublicKey::from_bytes(pk_bytes)
                .map_err(|e| anyhow!("invalid public key: {:?}", e))?,
        );
        sigs.push(
            BlstSignature::from_bytes(sig_bytes)
                .map_err(|e| anyhow!("invalid signature: {:?}", e))?,
        );
        msgs.push(msg);
    }

    let pk_refs: Vec<&BlstPublicKey> = pks.iter().collect();
    let sig_refs: Vec<&BlstSignature> = sigs.iter().collect();

    // Generate random 64-bit scalars for the linear combination
    use rand::RngCore;
    let mut rng = rand::thread_rng();
    let rands: Vec<blst::blst_scalar> = (0..verifications.len())
        .map(|_| {
            let mut scalar = blst::blst_scalar::default();
            let mut raw = [0u8; 8];
            rng.fill_bytes(&mut raw);
            raw[0] |= 1; // ensure nonzero
            scalar.b[..8].copy_from_slice(&raw);
            scalar
        })
        .collect();

    let err = BlstSignature::verify_multiple_aggregate_signatures(
        &msgs, DST, &pk_refs, true, &sig_refs, true, &rands, 64,
    );

    Ok(err == BLST_ERROR::BLST_SUCCESS)
}

/// BLS Aggregated Signature Verification
///
/// Verifies that an aggregated signature is valid for the given
/// aggregated public key and message.
///
/// NOTE: BLS signatures on G2 are malleable (negation of a valid signature is also valid).
/// This is acceptable in Aether because vote deduplication is based on
/// (slot, phase, block_hash, validator_address), not on signature content.
/// A malleable signature cannot be used to double-count a vote.
///
/// Verification equation: e(agg_pk, H(m)) == e(G1, agg_sig)
/// where:
/// - e() is the pairing function (Ate pairing on BLS12-381)
/// - agg_pk is the aggregated public key (G1 point)
/// - H(m) is the message hashed to G2
/// - agg_sig is the aggregated signature (G2 point)
/// - G1 is the generator of G1
///
/// This single pairing check verifies ALL individual signatures
/// that were aggregated, which is why BLS is so efficient.
///
/// Verify an aggregated BLS signature
///
/// Parameters:
/// - aggregated_pubkey: The sum of all signers' public keys (48 bytes)
/// - message: The message that was signed
/// - aggregated_signature: The sum of all signatures (96 bytes)
///
/// Returns: true if signature is valid, false otherwise
#[must_use = "discarding an aggregated verification result is a security bug"]
pub fn verify_aggregated(
    aggregated_pubkey: &[u8],
    message: &[u8],
    aggregated_signature: &[u8],
) -> Result<bool> {
    // Validate inputs
    if aggregated_pubkey.len() != 48 {
        anyhow::bail!("aggregated public key must be 48 bytes");
    }

    if aggregated_signature.len() != 96 {
        anyhow::bail!("aggregated signature must be 96 bytes");
    }

    let pk = BlstPublicKey::from_bytes(aggregated_pubkey)
        .map_err(|e| anyhow!("invalid aggregated public key: {:?}", e))?;
    let sig = BlstSignature::from_bytes(aggregated_signature)
        .map_err(|e| anyhow!("invalid aggregated signature: {:?}", e))?;

    Ok(sig.verify(true, message, DST, &[], &pk, true) == BLST_ERROR::BLST_SUCCESS)
}

/// Verify multiple aggregated signatures in batch
///
/// More efficient than verifying each individually when you have
/// many aggregated signatures to verify (e.g., from different blocks).
///
/// Uses batch pairing techniques to amortize the cost.
#[must_use = "verification results must not be silently discarded"]
pub fn batch_verify_aggregated(
    verifications: &[(Vec<u8>, Vec<u8>, Vec<u8>)], // (pubkey, message, signature)
) -> Result<Vec<bool>> {
    verifications
        .par_iter()
        .map(|(pubkey, message, signature)| verify_aggregated(pubkey, message, signature))
        .collect::<Result<Vec<bool>>>()
}

/// Verify an aggregated BLS signature with proof-of-possession enforcement.
///
/// Proof-of-possession (PoP) prevents rogue key attacks: each signer must
/// prove they know the secret key for their public key before their key
/// can be included in aggregation.
///
/// This function first verifies each individual PoP, then aggregates the
/// public keys and verifies the aggregated signature.
#[must_use = "discarding a PoP-verified aggregation result is a security bug"]
pub fn verify_aggregated_with_pop(
    individual_pubkeys: &[Vec<u8>],
    pop_signatures: &[Vec<u8>],
    message: &[u8],
    aggregated_signature: &[u8],
) -> Result<bool> {
    if individual_pubkeys.len() != pop_signatures.len() {
        anyhow::bail!("pubkey count must match PoP count");
    }
    if individual_pubkeys.is_empty() {
        anyhow::bail!("cannot verify with empty signer set");
    }

    // Verify each PoP first — reject if any signer hasn't proven key ownership
    for (i, (pk, pop)) in individual_pubkeys
        .iter()
        .zip(pop_signatures.iter())
        .enumerate()
    {
        match crate::keypair::verify_pop(pk, pop)? {
            true => {}
            false => anyhow::bail!("invalid proof-of-possession for signer {}", i),
        }
    }

    // All PoPs valid — aggregate pubkeys and verify the aggregate signature
    let agg_pk = crate::aggregate::aggregate_public_keys(individual_pubkeys)?;
    verify_aggregated(&agg_pk, message, aggregated_signature)
}
