use crate::chain_config::FeeParams;
use crate::primitives::{Address, PublicKey, Signature, H256};
use aether_crypto_primitives::ed25519;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const TRANSFER_PROGRAM_ID: H256 = H256([1u8; 32]);

/// Program id for deploying a WASM contract. `data` is `bincode(DeployPayload)`.
pub const DEPLOY_PROGRAM_ID: H256 = H256([2u8; 32]);

/// Program id for calling a deployed WASM contract. `data` is `bincode(CallPayload)`.
pub const CALL_PROGRAM_ID: H256 = H256([3u8; 32]);

/// Payload of a contract deployment transaction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeployPayload {
    /// WASM module bytes. Must export `execute(ptr: i32, len: i32) -> i32`.
    pub code: Vec<u8>,
}

/// Payload of a contract call transaction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallPayload {
    /// Address of the deployed contract.
    pub contract: Address,
    /// Opaque input passed to the contract's `execute` entry point.
    pub input: Vec<u8>,
}

/// Deterministic address of a contract deployed by `deployer` with `nonce`:
/// the first 20 bytes of `SHA-256("aether-contract" || deployer || nonce_le)`.
pub fn contract_address(deployer: &Address, nonce: u64) -> Address {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"aether-contract");
    hasher.update(deployer.as_bytes());
    hasher.update(nonce.to_le_bytes());
    let digest = hasher.finalize();
    let mut out = [0u8; 20];
    out.copy_from_slice(&digest[..20]);
    Address::from(out)
}

// Legacy chain ID constants -- prefer ChainConfig presets for new code.
pub const MAINNET_CHAIN_ID: u64 = 1;
pub const TESTNET_CHAIN_ID: u64 = 100;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transaction {
    pub nonce: u64,
    pub chain_id: u64,
    pub sender: Address,
    pub sender_pubkey: PublicKey,
    pub inputs: Vec<UtxoId>,
    pub outputs: Vec<UtxoOutput>,
    pub reads: HashSet<Address>,
    pub writes: HashSet<Address>,
    pub program_id: Option<H256>,
    pub data: Vec<u8>,
    pub gas_limit: u64,
    pub fee: u128,
    pub signature: Signature,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UtxoId {
    pub tx_hash: H256,
    pub output_index: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UtxoOutput {
    pub amount: u128,
    pub owner: PublicKey,
    pub script_hash: Option<H256>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransferPayload {
    pub recipient: Address,
    pub amount: u128,
    pub memo: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransactionReceipt {
    pub tx_hash: H256,
    pub block_hash: H256,
    pub slot: u64,
    pub status: TransactionStatus,
    pub gas_used: u64,
    pub logs: Vec<Log>,
    pub state_root: H256,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TransactionStatus {
    Success,
    Failed { reason: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Log {
    pub address: Address,
    pub topics: Vec<H256>,
    pub data: Vec<u8>,
}

impl Transaction {
    /// Transaction id: SHA-256 of the canonical encoding without signature.
    ///
    /// `reads`/`writes` are hash sets whose iteration order depends on a
    /// per-process random seed, so they are hashed in sorted order. (Hashing
    /// the sets as-is gave a different id in the signer's process than in
    /// the validators' processes for any tx with 2+ entries, so signatures
    /// failed at random on a real multi-process network.)
    pub fn hash(&self) -> H256 {
        use sha2::{Digest, Sha256};

        #[derive(Serialize)]
        struct Canonical<'a> {
            nonce: u64,
            chain_id: u64,
            sender: &'a Address,
            sender_pubkey: &'a PublicKey,
            inputs: &'a Vec<UtxoId>,
            outputs: &'a Vec<UtxoOutput>,
            reads: Vec<&'a Address>,
            writes: Vec<&'a Address>,
            program_id: &'a Option<H256>,
            data: &'a Vec<u8>,
            gas_limit: u64,
            fee: u128,
            signature: Signature,
        }
        let mut reads: Vec<&Address> = self.reads.iter().collect();
        reads.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        let mut writes: Vec<&Address> = self.writes.iter().collect();
        writes.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        let canonical = Canonical {
            nonce: self.nonce,
            chain_id: self.chain_id,
            sender: &self.sender,
            sender_pubkey: &self.sender_pubkey,
            inputs: &self.inputs,
            outputs: &self.outputs,
            reads,
            writes,
            program_id: &self.program_id,
            data: &self.data,
            gas_limit: self.gas_limit,
            fee: self.fee,
            signature: Signature::from_bytes(vec![]),
        };
        // bincode::serialize on a valid struct cannot fail;
        // SHA256 always produces 32 bytes matching H256.
        let bytes = bincode::serialize(&canonical).expect("tx serialization infallible");
        let hash = Sha256::digest(&bytes);
        H256::from_slice(&hash).expect("SHA256 produces 32 bytes")
    }

    /// Validate that the transaction's chain_id matches the expected network.
    pub fn validate_chain_id(&self, expected: u64) -> anyhow::Result<()> {
        if self.chain_id != expected {
            anyhow::bail!(
                "chain_id mismatch: tx has {}, expected {}",
                self.chain_id,
                expected
            );
        }
        Ok(())
    }

    pub fn verify_signature(&self) -> anyhow::Result<()> {
        if self.signature.as_bytes().is_empty() {
            anyhow::bail!("signature is empty");
        }
        // M3: Bound signature size to prevent DoS via oversized signatures
        if self.signature.as_bytes().len() > 128 {
            anyhow::bail!(
                "signature too large: {} bytes (max 128)",
                self.signature.as_bytes().len()
            );
        }

        // Verify the sender address matches the public key
        let derived_address = self.sender_pubkey.to_address();
        if derived_address != self.sender {
            anyhow::bail!("sender address does not match public key");
        }

        // Get the message to verify (transaction hash without signature)
        let msg = self.hash();

        ed25519::verify(
            self.sender_pubkey.as_bytes(),
            msg.as_bytes(),
            self.signature.as_bytes(),
        )
        .map_err(|e| anyhow::anyhow!("signature verification failed: {e:?}"))
    }

    pub fn calculate_fee(&self, fee_params: &FeeParams) -> anyhow::Result<u128> {
        let bytes = bincode::serialize(self)
            .map_err(|e| anyhow::anyhow!("serialize failed: {}", e))?
            .len() as u128;

        let byte_cost = fee_params
            .b
            .checked_mul(bytes)
            .ok_or_else(|| anyhow::anyhow!("fee overflow: B*bytes"))?;
        let gas_cost = fee_params
            .c
            .checked_mul(self.gas_limit as u128)
            .ok_or_else(|| anyhow::anyhow!("fee overflow: C*gas"))?;
        let computed_fee = fee_params
            .a
            .checked_add(byte_cost)
            .and_then(|v| v.checked_add(gas_cost))
            .ok_or_else(|| anyhow::anyhow!("fee calculation overflow"))?;

        if self.fee < computed_fee {
            anyhow::bail!(
                "fee too low: provided {}, required {}",
                self.fee,
                computed_fee
            );
        }

        Ok(self.fee)
    }

    pub fn ed25519_tuple(&self) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let msg = self.hash();
        (
            self.sender_pubkey.as_bytes().to_vec(),
            msg.as_bytes().to_vec(),
            self.signature.as_bytes().to_vec(),
        )
    }

    pub fn conflicts_with(&self, other: &Transaction) -> bool {
        // Write-Write conflict
        if !self.writes.is_disjoint(&other.writes) {
            return true;
        }
        // Write-Read conflicts (both directions)
        if !self.writes.is_disjoint(&other.reads) {
            return true;
        }
        if !other.writes.is_disjoint(&self.reads) {
            return true;
        }
        // UTxO conflicts
        for input in &self.inputs {
            if other.inputs.contains(input) {
                return true;
            }
        }
        false
    }
}

// ============================================================
// Blob Transactions (EIP-4844 style for AI data)
// ============================================================

/// A blob-carrying transaction for large AI data payloads.
///
/// Blobs are committed via KZG and pruned after `BLOB_RETENTION_SLOTS`.
/// The blob data itself is NOT stored in the block — only the KZG
/// commitment is included. Full blob data is served via the DA layer.
///
/// Fee model: separate blob fee market (base_fee + per-blob surcharge).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlobTransaction {
    /// Standard transaction fields.
    pub nonce: u64,
    pub chain_id: u64,
    pub sender: Address,
    pub sender_pubkey: PublicKey,
    pub gas_limit: u64,
    pub fee: u128,
    pub signature: Signature,

    /// KZG commitment to each blob (48 bytes each, compressed G1 point).
    pub blob_commitments: Vec<Vec<u8>>,

    /// Number of blobs attached.
    pub blob_count: u32,

    /// Total blob size in bytes (for fee calculation).
    pub total_blob_size: u64,

    /// Optional: program to invoke with blob data as input.
    pub program_id: Option<H256>,

    /// Auxiliary data (e.g., job ID, model hash for AI workloads).
    pub data: Vec<u8>,
}

/// How many slots blobs are retained before pruning.
pub const BLOB_RETENTION_SLOTS: u64 = 4096; // ~34 minutes at 500ms slots

/// Maximum blobs per transaction.
pub const MAX_BLOBS_PER_TX: u32 = 6;

/// Maximum blob size (128 KB, matching EIP-4844).
pub const MAX_BLOB_SIZE: u64 = 128 * 1024;

impl BlobTransaction {
    pub fn hash(&self) -> H256 {
        use sha2::{Digest, Sha256};
        let mut tx = self.clone();
        tx.signature = Signature::from_bytes(vec![]);
        let bytes = bincode::serialize(&tx).expect("blob tx serialization infallible");
        H256::from_slice(&Sha256::digest(&bytes)).expect("SHA256 produces 32 bytes")
    }

    /// Validate blob transaction constraints.
    pub fn validate(&self) -> Result<(), String> {
        if self.blob_count == 0 {
            return Err("blob transaction must have at least one blob".into());
        }
        if self.blob_count > MAX_BLOBS_PER_TX {
            return Err(format!(
                "too many blobs: {} > max {}",
                self.blob_count, MAX_BLOBS_PER_TX
            ));
        }
        if self.total_blob_size > self.blob_count as u64 * MAX_BLOB_SIZE {
            return Err("total blob size exceeds maximum".into());
        }
        if self.blob_commitments.len() != self.blob_count as usize {
            return Err("blob commitment count mismatch".into());
        }
        for (i, commitment) in self.blob_commitments.iter().enumerate() {
            if commitment.len() != 48 {
                return Err(format!(
                    "blob commitment {} has wrong size: {} (expected 48)",
                    i,
                    commitment.len()
                ));
            }
        }
        Ok(())
    }

    /// Calculate the blob fee (separate from execution gas fee).
    pub fn blob_fee(&self, fee_params: &FeeParams) -> u128 {
        fee_params
            .blob_per_blob_fee
            .saturating_mul(self.blob_count as u128)
            .saturating_add(
                fee_params
                    .blob_per_byte_fee
                    .saturating_mul(self.total_blob_size as u128),
            )
    }
}
