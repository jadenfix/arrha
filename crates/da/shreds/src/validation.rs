use anyhow::{bail, Result};

use crate::shred::Shred;

/// Validate a shred: payload hash, Ed25519 signature against proposer key, and freshness.
pub fn validate_shred(
    shred: &Shred,
    current_slot: u64,
    max_slot_age: u64,
    proposer_pubkey: &[u8],
) -> Result<()> {
    if shred.payload_hash != Shred::hash_payload(&shred.payload) {
        bail!("payload hash mismatch");
    }

    if shred.signature.as_bytes().is_empty() {
        bail!("missing signature");
    }

    // Verify Ed25519 signature against the proposer's public key
    let msg = shred.signing_message();
    aether_crypto_primitives::verify(proposer_pubkey, &msg, shred.signature.as_bytes())
        .map_err(|e| anyhow::anyhow!("invalid shred signature: {}", e))?;

    if shred.slot.saturating_add(max_slot_age) < current_slot {
        bail!("stale shred");
    }

    Ok(())
}
