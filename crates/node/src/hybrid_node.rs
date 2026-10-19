// ============================================================================
// HYBRID NODE - Phase 1 Full Integration Helper
// ============================================================================
// Wires VRF+HotStuff+BLS consensus into the node
// ============================================================================

use aether_consensus::HybridConsensus;
use aether_crypto_bls::BlsKeypair;
use aether_crypto_primitives::Keypair;
use aether_crypto_vrf::VrfKeypair;
use aether_types::{Address, PublicKey, ValidatorInfo};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Keypair bundle for a validator in hybrid consensus
pub struct ValidatorKeypair {
    pub ed25519: Keypair,
    pub vrf: VrfKeypair,
    pub bls: BlsKeypair,
}

impl ValidatorKeypair {
    /// Generate a new set of validator keys
    pub fn generate() -> Self {
        Self {
            ed25519: Keypair::generate(),
            vrf: VrfKeypair::generate(),
            bls: BlsKeypair::generate(),
        }
    }

    /// Get the validator's address
    pub fn address(&self) -> Address {
        let pubkey = PublicKey::from_bytes(self.ed25519.public_key());
        pubkey.to_address()
    }

    /// Get the validator's public key
    pub fn public_key(&self) -> PublicKey {
        PublicKey::from_bytes(self.ed25519.public_key())
    }

    /// Save keys to a JSON file on disk.
    pub fn save_to_file(&self, path: &Path) -> Result<()> {
        let keyfile = KeyFile {
            ed25519_secret: to_hex(&self.ed25519.secret_key()),
            bls_secret: to_hex(&self.bls.secret_key()),
            vrf_secret: to_hex(&self.vrf.secret_bytes()),
        };
        let json = serde_json::to_string_pretty(&keyfile)?;

        // Create the temporary file with owner-only permissions from its first
        // byte, then atomically replace the key file after syncing its contents.
        use std::io::Write;
        let file_name = path
            .file_name()
            .context("validator key path has no filename")?;
        let temporary = path.with_file_name(format!(
            ".{}.{}.tmp",
            file_name.to_string_lossy(),
            std::process::id()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("creating private key file: {}", temporary.display()))?;
        let result = (|| -> Result<()> {
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&temporary, path)?;
            #[cfg(unix)]
            {
                let directory = path
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                std::fs::File::open(directory)?.sync_all()?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.with_context(|| format!("persisting key file: {}", path.display()))?;

        Ok(())
    }

    /// Load keys from a JSON file.
    pub fn load_from_file(path: &Path) -> Result<Self> {
        let json = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read key file: {}", path.display()))?;
        let keyfile: KeyFile =
            serde_json::from_str(&json).with_context(|| "failed to parse key file JSON")?;

        let ed25519_bytes =
            from_hex(&keyfile.ed25519_secret).with_context(|| "invalid hex in ed25519_secret")?;
        let bls_bytes =
            from_hex(&keyfile.bls_secret).with_context(|| "invalid hex in bls_secret")?;
        let vrf_bytes =
            from_hex(&keyfile.vrf_secret).with_context(|| "invalid hex in vrf_secret")?;

        let ed25519 = Keypair::from_bytes(&ed25519_bytes)
            .map_err(|e| anyhow::anyhow!("invalid ed25519 key: {:?}", e))?;
        let bls = BlsKeypair::from_secret(bls_bytes)?;
        let vrf = VrfKeypair::from_secret(&vrf_bytes)?;

        Ok(ValidatorKeypair { ed25519, vrf, bls })
    }
}

/// On-disk key file format (JSON).
#[derive(Serialize, Deserialize)]
struct KeyFile {
    ed25519_secret: String,
    bls_secret: String,
    vrf_secret: String,
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn from_hex(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        anyhow::bail!("hex string has odd length");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|e| anyhow::anyhow!("invalid hex at position {}: {}", i, e))
        })
        .collect()
}

/// Create a hybrid consensus engine with VRF + HotStuff + BLS
pub fn create_hybrid_consensus(
    validators: Vec<ValidatorInfo>,
    my_keypair: Option<&ValidatorKeypair>,
    tau: f64,
    epoch_length: u64,
) -> Result<HybridConsensus> {
    let (my_vrf, my_bls, my_addr) = if let Some(kp) = my_keypair {
        (
            Some(kp.vrf.clone()),
            Some(kp.bls.clone()),
            Some(kp.address()),
        )
    } else {
        (None, None, None)
    };

    let mut consensus =
        HybridConsensus::new(validators, tau, epoch_length, my_vrf, my_bls, my_addr);

    // Register the local validator's BLS key so its own votes are accepted
    if let Some(kp) = my_keypair {
        let addr = kp.address();
        let bls_pk = kp.bls.public_key();
        let pop = kp.bls.proof_of_possession();
        if let Err(e) = consensus.register_bls_pubkey(addr, bls_pk, &pop) {
            tracing::warn!(err = %e, "failed to register local BLS key");
        }
    }

    Ok(consensus)
}

/// Create a hybrid consensus engine with VRF and BLS public keys for multi-validator verification.
pub fn create_hybrid_consensus_with_vrf_keys(
    validators: Vec<ValidatorInfo>,
    vrf_pubkeys: Vec<(Address, [u8; 32])>,
    my_keypair: Option<&ValidatorKeypair>,
    tau: f64,
    epoch_length: u64,
) -> Result<HybridConsensus> {
    let (my_vrf, my_bls, my_addr) = if let Some(kp) = my_keypair {
        (
            Some(kp.vrf.clone()),
            Some(kp.bls.clone()),
            Some(kp.address()),
        )
    } else {
        (None, None, None)
    };

    let mut consensus =
        HybridConsensus::new(validators, tau, epoch_length, my_vrf, my_bls, my_addr);

    // Register all VRF public keys for cross-validation
    for (addr, vrf_pk) in vrf_pubkeys {
        consensus.register_vrf_pubkey(addr, vrf_pk);
    }

    // Register the local validator's BLS key so votes are accepted
    if let Some(kp) = my_keypair {
        let addr = kp.address();
        let bls_pk = kp.bls.public_key();
        let pop = kp.bls.proof_of_possession();
        if let Err(e) = consensus.register_bls_pubkey(addr, bls_pk, &pop) {
            tracing::warn!(err = %e, "failed to register local BLS key");
        }
    }

    Ok(consensus)
}

/// Create a hybrid consensus with ALL validators' BLS keys registered.
/// Required for multi-validator E2E scenarios where each node needs to verify
/// votes from every other validator.
pub fn create_hybrid_consensus_with_all_keys(
    validators: Vec<ValidatorInfo>,
    vrf_pubkeys: Vec<(Address, [u8; 32])>,
    bls_pubkeys: Vec<(Address, Vec<u8>, Vec<u8>)>, // (addr, pubkey, pop_signature)
    my_keypair: Option<&ValidatorKeypair>,
    tau: f64,
    epoch_length: u64,
) -> Result<HybridConsensus> {
    let mut consensus = create_hybrid_consensus_with_vrf_keys(
        validators,
        vrf_pubkeys,
        my_keypair,
        tau,
        epoch_length,
    )?;

    // Register ALL validators' BLS keys for cross-validation
    for (addr, bls_pk, pop) in bls_pubkeys {
        if let Err(e) = consensus.register_bls_pubkey(addr, bls_pk, &pop) {
            tracing::warn!(validator = ?addr, err = %e, "failed to register BLS key");
        }
    }

    Ok(consensus)
}

/// Helper to create validator info from a keypair
pub fn validator_info_from_keypair(keypair: &ValidatorKeypair, stake: u128) -> ValidatorInfo {
    ValidatorInfo {
        pubkey: keypair.public_key(),
        stake,
        commission: 1000, // 10%
        active: true,
    }
}
