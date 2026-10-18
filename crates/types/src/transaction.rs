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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::{PublicKey as TxPublicKey, Signature as TxSignature, H160};
    use aether_crypto_primitives::Keypair;
    use std::collections::HashSet;

    fn signed_transaction(keypair: &Keypair) -> Transaction {
        let address = H160::from_slice(&keypair.to_address()).unwrap();
        let mut tx = Transaction {
            nonce: 0,
            chain_id: TESTNET_CHAIN_ID,
            sender: address,
            sender_pubkey: TxPublicKey::from_bytes(keypair.public_key()),
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 100,
            signature: TxSignature::from_bytes(vec![]),
        };

        let hash = tx.hash();
        let signature = keypair.sign(hash.as_bytes());
        tx.signature = TxSignature::from_bytes(signature);
        tx
    }

    #[test]
    fn hash_is_independent_of_set_iteration_order() {
        let keypair = Keypair::generate();
        let mut tx = signed_transaction(&keypair);
        for i in 0..16u8 {
            tx.writes.insert(H160([i; 20]));
            tx.reads.insert(H160([i.wrapping_add(100); 20]));
        }
        // A deserialized copy gets fresh HashSets with a different random
        // iteration order, as on a remote validator.
        let bytes = bincode::serialize(&tx).unwrap();
        for _ in 0..8 {
            let copy: Transaction = bincode::deserialize(&bytes).unwrap();
            let rebuilt = Transaction {
                reads: copy.reads.iter().cloned().collect(),
                writes: copy.writes.iter().cloned().collect(),
                ..copy
            };
            assert_eq!(rebuilt.hash(), tx.hash());
        }
    }

    #[test]
    fn verifies_valid_signature() {
        let keypair = Keypair::generate();
        let tx = signed_transaction(&keypair);
        assert!(tx.verify_signature().is_ok());
    }

    #[test]
    fn rejects_tampered_signature() {
        let keypair = Keypair::generate();
        let mut tx = signed_transaction(&keypair);
        tx.signature = TxSignature::from_bytes(vec![0; 64]);
        assert!(tx.verify_signature().is_err());
    }

    #[test]
    fn test_chain_id_in_hash() {
        let keypair = Keypair::generate();
        let address = H160::from_slice(&keypair.to_address()).unwrap();
        let tx1 = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: address,
            sender_pubkey: TxPublicKey::from_bytes(keypair.public_key()),
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 100,
            signature: TxSignature::from_bytes(vec![]),
        };
        let mut tx2 = tx1.clone();
        tx2.chain_id = 2;

        assert_ne!(
            tx1.hash(),
            tx2.hash(),
            "Different chain_id must produce different hash"
        );
    }

    #[test]
    fn test_validate_chain_id() {
        let keypair = Keypair::generate();
        let address = H160::from_slice(&keypair.to_address()).unwrap();
        let tx = Transaction {
            nonce: 0,
            chain_id: 100,
            sender: address,
            sender_pubkey: TxPublicKey::from_bytes(keypair.public_key()),
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 100,
            signature: TxSignature::from_bytes(vec![]),
        };
        assert!(
            tx.validate_chain_id(100).is_ok(),
            "Matching chain_id should pass"
        );
        assert!(
            tx.validate_chain_id(1).is_err(),
            "Mismatched chain_id should fail"
        );
    }

    #[test]
    fn test_oversized_signature_rejected() {
        let keypair = Keypair::generate();
        let address = H160::from_slice(&keypair.to_address()).unwrap();
        let tx = Transaction {
            nonce: 0,
            chain_id: super::TESTNET_CHAIN_ID,
            sender: address,
            sender_pubkey: TxPublicKey::from_bytes(keypair.public_key()),
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 100,
            signature: TxSignature::from_bytes(vec![0u8; 256]), // 256 bytes > 128 limit
        };
        let result = tx.verify_signature();
        assert!(result.is_err(), "Oversized signature should be rejected");
        assert!(result.unwrap_err().to_string().contains("too large"));
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

#[cfg(test)]
mod blob_tests {
    use super::*;

    fn make_blob_tx(blob_count: u32, blob_size: u64) -> BlobTransaction {
        BlobTransaction {
            nonce: 0,
            chain_id: TESTNET_CHAIN_ID,
            sender: Address::from_slice(&[1u8; 20]).unwrap(),
            sender_pubkey: PublicKey::from_bytes(vec![2u8; 32]),
            gas_limit: 21000,
            fee: 100_000,
            signature: Signature::from_bytes(vec![0u8; 64]),
            blob_commitments: (0..blob_count).map(|_| vec![0u8; 48]).collect(),
            blob_count,
            total_blob_size: blob_size,
            program_id: None,
            data: vec![],
        }
    }

    #[test]
    fn test_valid_blob_tx() {
        let tx = make_blob_tx(2, 200_000);
        assert!(tx.validate().is_ok());
    }

    #[test]
    fn test_reject_zero_blobs() {
        let tx = make_blob_tx(0, 0);
        assert!(tx.validate().is_err());
    }

    #[test]
    fn test_reject_too_many_blobs() {
        let tx = make_blob_tx(MAX_BLOBS_PER_TX + 1, 100);
        assert!(tx.validate().is_err());
    }

    #[test]
    fn test_reject_commitment_count_mismatch() {
        let mut tx = make_blob_tx(3, 100);
        tx.blob_commitments.pop(); // Remove one commitment
        assert!(tx.validate().is_err());
    }

    #[test]
    fn test_blob_fee_calculation() {
        let tx = make_blob_tx(2, 200_000);
        let fee_params = crate::chain_config::ChainConfig::devnet().fees;
        let fee = tx.blob_fee(&fee_params);
        // 2 * 100_000 (per blob) + 200_000 * 1 (per byte)
        assert_eq!(fee, 400_000);
    }

    #[test]
    fn test_blob_hash_deterministic() {
        let tx = make_blob_tx(1, 1000);
        assert_eq!(tx.hash(), tx.hash());
    }

    #[test]
    fn test_blob_fee_no_overflow_large_params() {
        // Verify that blob_fee saturates rather than wrapping on extreme inputs.
        let tx = BlobTransaction {
            nonce: 0,
            chain_id: TESTNET_CHAIN_ID,
            sender: Address::from_slice(&[1u8; 20]).unwrap(),
            sender_pubkey: PublicKey::from_bytes(vec![2u8; 32]),
            gas_limit: 21000,
            fee: 0,
            signature: Signature::from_bytes(vec![0u8; 64]),
            blob_commitments: vec![vec![0u8; 48]],
            blob_count: u32::MAX,
            total_blob_size: u64::MAX,
            program_id: None,
            data: vec![],
        };

        // fee_params with max fee values
        let mut fee_params = crate::chain_config::ChainConfig::devnet().fees;
        fee_params.blob_per_blob_fee = u128::MAX;
        fee_params.blob_per_byte_fee = u128::MAX;

        // Should saturate to u128::MAX rather than overflow/panic
        let fee = tx.blob_fee(&fee_params);
        assert_eq!(fee, u128::MAX, "blob_fee must saturate on overflow");
    }
}
