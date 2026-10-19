//! Live-chain helpers: build and sign transfer / contract transactions with
//! the correct chain id and fee, submit them, and wait for finality.
//!
//! ```no_run
//! # async fn demo() -> Result<(), aether_sdk::AetherSdkError> {
//! use aether_sdk::{chain, AetherClient};
//! use aether_crypto_primitives::Keypair;
//! let client = AetherClient::new("http://127.0.0.1:8545");
//! let funder = Keypair::devnet(0);
//! let chain_id = client.chain_id().await?;
//! let nonce = client.next_nonce(&funder).await?;
//! let fees = client.fee_parameters().await?;
//! let tx = chain::transfer_tx_with_fee_parameters(
//!     &funder, chain_id, nonce, [7u8; 20].into(), 1_000, &fees,
//! )?;
//! let hash = client.send_transaction(&tx).await?;
//! let status = client.wait_for_finality(hash, std::time::Duration::from_secs(30)).await?;
//! println!("{status}");
//! # Ok(()) }
//! ```

use std::collections::HashSet;
use std::time::{Duration, Instant};

use aether_crypto_primitives::Keypair;
use aether_types::{
    contract_address, Address, CallPayload, ChainConfig, DeployPayload, PublicKey, Signature,
    Transaction, TransferPayload, CALL_PROGRAM_ID, DEPLOY_PROGRAM_ID, H256, TRANSFER_PROGRAM_ID,
};
use serde_json::{json, Value};

use crate::client::AetherClient;
use crate::error::AetherSdkError;

/// Gas limit used for plain transfers.
pub const TRANSFER_GAS: u64 = 21_000;
/// Default gas limit (WASM fuel) for contract calls.
pub const CALL_GAS: u64 = 100_000;
/// Default gas limit for contract deployment.
pub const DEPLOY_GAS: u64 = 200_000;

/// Fee coefficients reported by a live node. The `chain_id` prevents accidentally
/// signing with fee values from a different network; `min_base_fee` is exposed
/// for callers but the current transaction fee rule uses only a, b, and c.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainFeeParameters {
    pub a: u128,
    pub b: u128,
    pub c: u128,
    pub min_base_fee: u128,
    pub chain_id: u64,
}

/// Address of a keypair.
pub fn address_of(keypair: &Keypair) -> Address {
    Address::from(keypair.to_address())
}

/// Sign `tx` using twice the minimum calculated from devnet fee parameters.
/// This compatibility helper is for offline/devnet use. For another network,
/// fetch `AetherClient::fee_parameters` and use
/// [`finalize_tx_with_fee_parameters`].
pub fn finalize_tx(keypair: &Keypair, mut tx: Transaction) -> Transaction {
    let fees = ChainConfig::devnet().fees;
    // Compatibility helper for offline/devnet callers. Online callers should
    // use `finalize_tx_with_fee_parameters` with a node response.
    tx.fee =
        recommended_fee(&tx, fees.a, fees.b, fees.c).expect("devnet fee calculation must fit u128");
    let hash = tx.hash();
    tx.signature = Signature::from_bytes(keypair.sign(hash.as_bytes()));
    tx
}

/// Price and sign a transaction with fee parameters fetched from the target node.
pub fn finalize_tx_with_fee_parameters(
    keypair: &Keypair,
    mut tx: Transaction,
    fees: &ChainFeeParameters,
) -> Result<Transaction, AetherSdkError> {
    if tx.chain_id != fees.chain_id {
        return Err(AetherSdkError::InvalidFee(format!(
            "fee parameters are for chain {}, transaction is for chain {}",
            fees.chain_id, tx.chain_id
        )));
    }
    tx.fee = recommended_fee(&tx, fees.a, fees.b, fees.c)?;
    let hash = tx.hash();
    tx.signature = Signature::from_bytes(keypair.sign(hash.as_bytes()));
    tx.verify_signature()
        .map_err(|e| AetherSdkError::InvalidSignature(e.to_string()))?;
    Ok(tx)
}

/// Check a signed transaction against live node fee coefficients without
/// changing its fee or signature.
pub(crate) fn validate_live_fee(
    tx: &Transaction,
    fees: &ChainFeeParameters,
) -> Result<(), AetherSdkError> {
    if tx.chain_id != fees.chain_id {
        return Err(AetherSdkError::InvalidFee(format!(
            "transaction chain id {} does not match node chain id {}",
            tx.chain_id, fees.chain_id
        )));
    }
    let required = minimum_fee(tx, fees.a, fees.b, fees.c)?;
    if tx.fee < required {
        return Err(AetherSdkError::InvalidFee(format!(
            "fee too low for live chain parameters: provided {}, required {}",
            tx.fee, required
        )));
    }
    Ok(())
}

pub(crate) fn recommended_fee_for_transaction(
    tx: &Transaction,
    fees: &ChainFeeParameters,
) -> Result<u128, AetherSdkError> {
    if tx.chain_id != fees.chain_id {
        return Err(AetherSdkError::InvalidFee(
            "fee parameter chain id mismatch".into(),
        ));
    }
    recommended_fee(tx, fees.a, fees.b, fees.c)
}

pub(crate) fn minimum_fee_for_validation(
    tx: &Transaction,
    fees: &ChainFeeParameters,
) -> Result<u128, AetherSdkError> {
    if tx.chain_id != fees.chain_id {
        return Err(AetherSdkError::InvalidFee(
            "fee parameter chain id mismatch".into(),
        ));
    }
    minimum_fee(tx, fees.a, fees.b, fees.c)
}

fn recommended_fee(tx: &Transaction, a: u128, b: u128, c: u128) -> Result<u128, AetherSdkError> {
    minimum_fee(tx, a, b, c)?
        .checked_mul(2)
        .ok_or_else(|| AetherSdkError::InvalidFee("2x recommended fee overflow".into()))
}

fn minimum_fee(tx: &Transaction, a: u128, b: u128, c: u128) -> Result<u128, AetherSdkError> {
    // Reserve the final Ed25519 signature's serialized size before signing.
    let mut sized = tx.clone();
    sized.signature = Signature::from_bytes(vec![0; 64]);
    let bytes = bincode::serialized_size(&sized).map_err(AetherSdkError::serialization)? as u128;
    let byte_fee = b
        .checked_mul(bytes)
        .ok_or_else(|| AetherSdkError::InvalidFee("B*serialized_size overflow".into()))?;
    let gas_fee = c
        .checked_mul(tx.gas_limit as u128)
        .ok_or_else(|| AetherSdkError::InvalidFee("C*gas_limit overflow".into()))?;
    a.checked_add(byte_fee)
        .and_then(|fee| fee.checked_add(gas_fee))
        .ok_or_else(|| AetherSdkError::InvalidFee("minimum fee overflow".into()))
}

fn base_tx(keypair: &Keypair, chain_id: u64, nonce: u64, gas_limit: u64) -> Transaction {
    let sender_pubkey = PublicKey::from_bytes(keypair.public_key());
    let sender = sender_pubkey.to_address();
    let mut reads = HashSet::new();
    reads.insert(sender);
    let mut writes = HashSet::new();
    writes.insert(sender);
    Transaction {
        nonce,
        chain_id,
        sender,
        sender_pubkey,
        inputs: vec![],
        outputs: vec![],
        reads,
        writes,
        program_id: None,
        data: vec![],
        gas_limit,
        fee: 0,
        signature: Signature::from_bytes(vec![]),
    }
}

/// Signed token transfer.
pub fn transfer_tx(
    keypair: &Keypair,
    chain_id: u64,
    nonce: u64,
    to: Address,
    amount: u128,
) -> Transaction {
    let fees = ChainConfig::devnet().fees;
    transfer_tx_with_coefficients(
        keypair,
        chain_id,
        nonce,
        to,
        amount,
        (fees.a, fees.b, fees.c),
    )
    .expect("devnet transfer fee calculation must fit u128")
}

/// Signed token transfer priced with the fee parameters reported by the target node.
pub fn transfer_tx_with_fee_parameters(
    keypair: &Keypair,
    chain_id: u64,
    nonce: u64,
    to: Address,
    amount: u128,
    fees: &ChainFeeParameters,
) -> Result<Transaction, AetherSdkError> {
    let mut tx = transfer_tx_unsigned(keypair, chain_id, nonce, to, amount);
    tx = finalize_tx_with_fee_parameters(keypair, tx, fees)?;
    Ok(tx)
}

fn transfer_tx_with_coefficients(
    keypair: &Keypair,
    chain_id: u64,
    nonce: u64,
    to: Address,
    amount: u128,
    coefficients: (u128, u128, u128),
) -> Result<Transaction, AetherSdkError> {
    let (a, b, c) = coefficients;
    let mut tx = transfer_tx_unsigned(keypair, chain_id, nonce, to, amount);
    tx.fee = recommended_fee(&tx, a, b, c)?;
    let hash = tx.hash();
    tx.signature = Signature::from_bytes(keypair.sign(hash.as_bytes()));
    Ok(tx)
}

fn transfer_tx_unsigned(
    keypair: &Keypair,
    chain_id: u64,
    nonce: u64,
    to: Address,
    amount: u128,
) -> Transaction {
    let mut tx = base_tx(keypair, chain_id, nonce, TRANSFER_GAS);
    tx.writes.insert(to);
    tx.program_id = Some(TRANSFER_PROGRAM_ID);
    tx.data = bincode::serialize(&TransferPayload {
        recipient: to,
        amount,
        memo: None,
    })
    .expect("transfer payload serializes");
    tx
}

/// Signed contract deployment. Returns the transaction and the address the
/// contract will live at.
pub fn deploy_tx(
    keypair: &Keypair,
    chain_id: u64,
    nonce: u64,
    code: Vec<u8>,
) -> (Transaction, Address) {
    let fees = ChainConfig::devnet().fees;
    deploy_tx_with_coefficients(keypair, chain_id, nonce, code, (fees.a, fees.b, fees.c))
        .expect("devnet deployment fee calculation must fit u128")
}

/// Signed contract deployment priced with fee parameters reported by the target node.
pub fn deploy_tx_with_fee_parameters(
    keypair: &Keypair,
    chain_id: u64,
    nonce: u64,
    code: Vec<u8>,
    fees: &ChainFeeParameters,
) -> Result<(Transaction, Address), AetherSdkError> {
    if chain_id != fees.chain_id {
        return Err(AetherSdkError::InvalidFee(
            "fee parameter chain id mismatch".into(),
        ));
    }
    let mut tx = deploy_tx_unsigned(keypair, chain_id, nonce, code);
    let address = contract_address(&tx.sender, nonce);
    tx = finalize_tx_with_fee_parameters(keypair, tx, fees)?;
    Ok((tx, address))
}

fn deploy_tx_with_coefficients(
    keypair: &Keypair,
    chain_id: u64,
    nonce: u64,
    code: Vec<u8>,
    coefficients: (u128, u128, u128),
) -> Result<(Transaction, Address), AetherSdkError> {
    let (a, b, c) = coefficients;
    let mut tx = deploy_tx_unsigned(keypair, chain_id, nonce, code);
    let address = contract_address(&tx.sender, nonce);
    tx.fee = recommended_fee(&tx, a, b, c)?;
    let hash = tx.hash();
    tx.signature = Signature::from_bytes(keypair.sign(hash.as_bytes()));
    Ok((tx, address))
}

fn deploy_tx_unsigned(keypair: &Keypair, chain_id: u64, nonce: u64, code: Vec<u8>) -> Transaction {
    let mut tx = base_tx(keypair, chain_id, nonce, DEPLOY_GAS);
    let address = contract_address(&tx.sender, nonce);
    tx.writes.insert(address);
    tx.program_id = Some(DEPLOY_PROGRAM_ID);
    tx.data = bincode::serialize(&DeployPayload { code }).expect("deploy payload serializes");
    tx
}

/// Signed contract call.
pub fn call_tx(
    keypair: &Keypair,
    chain_id: u64,
    nonce: u64,
    contract: Address,
    input: Vec<u8>,
    gas_limit: u64,
) -> Transaction {
    let fees = ChainConfig::devnet().fees;
    call_tx_with_coefficients(
        keypair,
        chain_id,
        nonce,
        contract,
        input,
        gas_limit,
        (fees.a, fees.b, fees.c),
    )
    .expect("devnet contract call fee calculation must fit u128")
}

/// Signed contract call priced with fee parameters reported by the target node.
pub fn call_tx_with_fee_parameters(
    keypair: &Keypair,
    chain_id: u64,
    nonce: u64,
    contract: Address,
    input: Vec<u8>,
    gas_limit: u64,
    fees: &ChainFeeParameters,
) -> Result<Transaction, AetherSdkError> {
    if chain_id != fees.chain_id {
        return Err(AetherSdkError::InvalidFee(
            "fee parameter chain id mismatch".into(),
        ));
    }
    let mut tx = call_tx_unsigned(keypair, chain_id, nonce, contract, input, gas_limit);
    tx = finalize_tx_with_fee_parameters(keypair, tx, fees)?;
    Ok(tx)
}

fn call_tx_with_coefficients(
    keypair: &Keypair,
    chain_id: u64,
    nonce: u64,
    contract: Address,
    input: Vec<u8>,
    gas_limit: u64,
    coefficients: (u128, u128, u128),
) -> Result<Transaction, AetherSdkError> {
    let (a, b, c) = coefficients;
    let mut tx = call_tx_unsigned(keypair, chain_id, nonce, contract, input, gas_limit);
    tx.fee = recommended_fee(&tx, a, b, c)?;
    let hash = tx.hash();
    tx.signature = Signature::from_bytes(keypair.sign(hash.as_bytes()));
    Ok(tx)
}

fn call_tx_unsigned(
    keypair: &Keypair,
    chain_id: u64,
    nonce: u64,
    contract: Address,
    input: Vec<u8>,
    gas_limit: u64,
) -> Transaction {
    let mut tx = base_tx(keypair, chain_id, nonce, gas_limit);
    tx.reads.insert(contract);
    tx.writes.insert(contract);
    tx.program_id = Some(CALL_PROGRAM_ID);
    tx.data =
        bincode::serialize(&CallPayload { contract, input }).expect("call payload serializes");
    tx
}

fn hex0x(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

impl AetherClient {
    /// Call any JSON-RPC method.
    pub async fn call_rpc(
        &self,
        method: &str,
        params: Vec<Value>,
    ) -> Result<Value, AetherSdkError> {
        self.rpc_call(method, &params).await
    }

    /// Numeric chain id reported by the node (`aeth_chainId`).
    pub async fn chain_id(&self) -> Result<u64, AetherSdkError> {
        let v = self.rpc_call("aeth_chainId", &[]).await?;
        let s = v
            .as_str()
            .ok_or_else(|| AetherSdkError::invalid_response("aeth_chainId: expected string"))?;
        u64::from_str_radix(s.trim_start_matches("0x"), 16)
            .map_err(|e| AetherSdkError::invalid_response(format!("aeth_chainId: {e}")))
    }

    /// Next nonce for `keypair`, counting transactions in not-yet-final blocks.
    pub async fn next_nonce(&self, keypair: &Keypair) -> Result<u64, AetherSdkError> {
        let addr = address_of(keypair);
        let v = self
            .rpc_call("aeth_getPendingAccount", &[json!(hex0x(addr.as_bytes()))])
            .await?;
        Ok(v.get("nonce").and_then(|n| n.as_u64()).unwrap_or(0))
    }

    /// Submit a signed transaction (`aeth_sendRawTransaction`).
    pub async fn send_transaction(&self, tx: &Transaction) -> Result<H256, AetherSdkError> {
        let submitted = self.submit(tx.clone()).await?;
        Ok(submitted.tx_hash)
    }

    /// Transaction status (`aeth_getTransactionStatus`): `unknown`,
    /// `included` (in a pending block) or `finalized`.
    pub async fn transaction_status(&self, hash: H256) -> Result<Value, AetherSdkError> {
        self.rpc_call(
            "aeth_getTransactionStatus",
            &[json!(hex0x(hash.as_bytes()))],
        )
        .await
    }

    /// Poll until the transaction is finalized or `timeout` elapses.
    pub async fn wait_for_finality(
        &self,
        hash: H256,
        timeout: Duration,
    ) -> Result<Value, AetherSdkError> {
        let start = Instant::now();
        loop {
            let status = self.transaction_status(hash).await?;
            if status.get("status").and_then(|s| s.as_str()) == Some("finalized") {
                if status.get("success") != Some(&Value::Bool(true)) {
                    return Err(AetherSdkError::invalid_response(format!(
                        "transaction {} finalized unsuccessfully: {status}",
                        hex0x(hash.as_bytes())
                    )));
                }
                return Ok(status);
            }
            if start.elapsed() > timeout {
                return Err(AetherSdkError::Timeout(format!(
                    "transaction {} not finalized after {:?} (last status: {status})",
                    hex0x(hash.as_bytes()),
                    timeout
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Contract storage as a hex map (`finalized` or `pending` view).
    pub async fn contract_storage(
        &self,
        contract: Address,
        pending: bool,
    ) -> Result<Value, AetherSdkError> {
        let view = if pending { "pending" } else { "finalized" };
        self.rpc_call(
            "aeth_getContractStorage",
            &[json!(hex0x(contract.as_bytes())), json!(view)],
        )
        .await
    }
}
