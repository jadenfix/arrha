use std::fs;
use std::io::Write;
use std::path::Path;

use aether_crypto_primitives::Keypair;
use aether_types::{Address, H256};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct KeyFile {
    pub secret_key: String,
    pub public_key: String,
    pub address: String,
}

pub struct KeyMaterial {
    pub keypair: Keypair,
    pub address: Address,
}

pub fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
    }
    Ok(())
}

pub fn write_key_file(path: &Path, keypair: &Keypair) -> Result<()> {
    ensure_parent_dir(path)?;
    let public_key = keypair.public_key();
    let secret_key = keypair.secret_key();
    let address_bytes = keypair.to_address();
    let address = Address::from_slice(&address_bytes)
        .map_err(|e| anyhow::anyhow!("failed to derive address from keypair: {e}"))?;
    let payload = KeyFile {
        secret_key: format!("0x{}", hex::encode(secret_key)),
        public_key: format!("0x{}", hex::encode(public_key)),
        address: format!("0x{}", hex::encode(address.as_bytes())),
    };

    let json = serde_json::to_vec_pretty(&payload)?;
    let mut file = fs::File::create(path)
        .with_context(|| format!("failed to create key file {}", path.display()))?;
    file.write_all(&json)
        .with_context(|| format!("failed to write key file {}", path.display()))?;
    file.flush()
        .with_context(|| format!("failed to flush key file {}", path.display()))?;
    Ok(())
}

pub fn read_key_file(path: &Path) -> Result<KeyMaterial> {
    let data = fs::read_to_string(path)
        .with_context(|| format!("failed to read key file {}", path.display()))?;
    let parsed: KeyFile = serde_json::from_str(&data)
        .with_context(|| format!("invalid key file {}", path.display()))?;

    let secret_bytes = decode_hex_32(&parsed.secret_key)?;
    let keypair = Keypair::from_bytes(&secret_bytes)
        .map_err(|_| anyhow!("failed to load secret key from {}", path.display()))?;

    // Derive public/address to validate integrity
    let derived_public = keypair.public_key();
    let derived_address_bytes = keypair.to_address();
    let derived_address = Address::from_slice(&derived_address_bytes).map_err(|_| {
        anyhow!(
            "failed to derive address from keypair in {}",
            path.display()
        )
    })?;

    if let Ok(expected_public) = decode_hex_vec(&parsed.public_key) {
        if expected_public != derived_public {
            return Err(anyhow!(
                "public key mismatch in key file {}; re-generate keys",
                path.display()
            ));
        }
    }

    if let Ok(expected_addr) = decode_hex_vec(&parsed.address) {
        if expected_addr != derived_address.as_bytes() {
            return Err(anyhow!(
                "address mismatch in key file {}; re-generate keys",
                path.display()
            ));
        }
    }

    Ok(KeyMaterial {
        keypair,
        address: derived_address,
    })
}

pub fn decode_hex_vec(input: &str) -> Result<Vec<u8>> {
    let trimmed = input.strip_prefix("0x").unwrap_or(input);
    hex::decode(trimmed).map_err(|err| anyhow!("invalid hex string: {err}"))
}

pub fn decode_hex_32(input: &str) -> Result<[u8; 32]> {
    let bytes = decode_hex_vec(input)?;
    if bytes.len() != 32 {
        return Err(anyhow!(
            "expected 32-byte secret key, found {} bytes",
            bytes.len()
        ));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(arr)
}

pub fn parse_address(input: &str) -> Result<Address> {
    let bytes = decode_hex_vec(input)?;
    Address::from_slice(&bytes).map_err(|_| {
        anyhow!("address must be 20 bytes (40 hex chars plus 0x prefix), e.g. 0x1234...abcd")
    })
}

pub fn parse_h256(input: &str) -> Result<H256> {
    let bytes = decode_hex_vec(input)?;
    H256::from_slice(&bytes)
        .map_err(|_| anyhow!("hash must be 32 bytes (64 hex chars plus 0x prefix)"))
}

pub fn address_to_string(address: &Address) -> String {
    format!("0x{}", hex::encode(address.as_bytes()))
}

pub fn h256_to_string(value: &H256) -> String {
    format!("0x{}", hex::encode(value.as_bytes()))
}
