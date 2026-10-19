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
//! let tx = chain::transfer_tx(&funder, chain_id, nonce, [7u8; 20].into(), 1_000);
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

/// Address of a keypair.
pub fn address_of(keypair: &Keypair) -> Address {
    Address::from(keypair.to_address())
}

/// Sign `tx` in place, setting the fee to twice the protocol minimum
/// (`a + b * bytes + c * gas`, devnet fee parameters).
pub fn finalize_tx(keypair: &Keypair, mut tx: Transaction) -> Transaction {
    let fees = ChainConfig::devnet().fees;
    // Fee is a fixed-width u128 in bincode, so the size does not depend on it.
    let bytes = bincode::serialized_size(&tx).unwrap_or(1024) as u128;
    let minimum = fees.a + fees.b * bytes + fees.c * tx.gas_limit as u128;
    tx.fee = minimum.saturating_mul(2);
    let hash = tx.hash();
    tx.signature = Signature::from_bytes(keypair.sign(hash.as_bytes()));
    tx
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
    let mut tx = base_tx(keypair, chain_id, nonce, TRANSFER_GAS);
    tx.writes.insert(to);
    tx.program_id = Some(TRANSFER_PROGRAM_ID);
    tx.data = bincode::serialize(&TransferPayload {
        recipient: to,
        amount,
        memo: None,
    })
    .expect("transfer payload serializes");
    finalize_tx(keypair, tx)
}

/// Signed contract deployment. Returns the transaction and the address the
/// contract will live at.
pub fn deploy_tx(
    keypair: &Keypair,
    chain_id: u64,
    nonce: u64,
    code: Vec<u8>,
) -> (Transaction, Address) {
    let mut tx = base_tx(keypair, chain_id, nonce, DEPLOY_GAS);
    let address = contract_address(&tx.sender, nonce);
    tx.writes.insert(address);
    tx.program_id = Some(DEPLOY_PROGRAM_ID);
    tx.data = bincode::serialize(&DeployPayload { code }).expect("deploy payload serializes");
    (finalize_tx(keypair, tx), address)
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
    let mut tx = base_tx(keypair, chain_id, nonce, gas_limit);
    tx.reads.insert(contract);
    tx.writes.insert(contract);
    tx.program_id = Some(CALL_PROGRAM_ID);
    tx.data =
        bincode::serialize(&CallPayload { contract, input }).expect("call payload serializes");
    finalize_tx(keypair, tx)
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
        let bytes = bincode::serialize(tx).map_err(AetherSdkError::serialization)?;
        let v = self
            .rpc_call("aeth_sendRawTransaction", &[json!(hex0x(&bytes))])
            .await?;
        let s = v
            .as_str()
            .ok_or_else(|| AetherSdkError::invalid_response("expected tx hash string"))?;
        let raw = hex::decode(s.trim_start_matches("0x"))
            .map_err(|e| AetherSdkError::invalid_response(format!("bad tx hash: {e}")))?;
        H256::from_slice(&raw).map_err(AetherSdkError::invalid_response)
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
