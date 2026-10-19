//! `aetherctl chain ...`: talk to a running node (devnet or otherwise):
//! transfers, WASM contract deploy/call, account/contract queries and a
//! small block explorer view. Signing keys come from `--key <file>` (see
//! `aetherctl keys generate`) or `--dev <i>` (the deterministic, publicly
//! known devnet accounts funded by `genesis-ceremony`).

use std::path::PathBuf;
use std::time::Duration;

use aether_crypto_primitives::Keypair;
use aether_sdk::{chain, AetherClient};
use aether_types::Address;
use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand};
use serde_json::{json, Value};

use crate::config::expand_path;
use crate::io::{parse_address, parse_h256, read_key_file};

#[derive(Args, Debug)]
pub struct ChainArgs {
    /// JSON-RPC endpoint of any validator
    #[arg(long, global = true, default_value = "http://127.0.0.1:8545")]
    pub rpc: String,

    #[command(subcommand)]
    pub command: ChainCommand,
}

#[derive(Args, Debug, Clone)]
pub struct Signer {
    /// Sign with a key file (from `aetherctl keys generate`)
    #[arg(long, conflicts_with = "dev")]
    pub key: Option<String>,
    /// Sign with deterministic devnet account <i> (local devnets only)
    #[arg(long)]
    pub dev: Option<u32>,
}

impl Signer {
    fn keypair(&self) -> Result<Keypair> {
        match (&self.key, self.dev) {
            (Some(path), _) => Ok(read_key_file(&expand_path(path)?)?.keypair),
            (None, Some(i)) => Ok(Keypair::devnet(i)),
            (None, None) => bail!("pass --key <file> or --dev <index>"),
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum ChainCommand {
    /// Node status: slots, finality, peers, mempool
    Info,
    /// Account balance and nonce (finalized and including pending blocks)
    Account {
        /// Address (0x...) or `dev:<i>`
        address: String,
    },
    /// Transfer tokens
    Transfer {
        #[command(flatten)]
        signer: Signer,
        #[arg(long)]
        to: String,
        #[arg(long)]
        amount: u128,
        /// Wait until the transaction is finalized
        #[arg(long)]
        wait: bool,
    },
    /// Deploy a WASM contract (.wasm, or .wat text format)
    Deploy {
        #[command(flatten)]
        signer: Signer,
        /// Path to the contract
        path: PathBuf,
        #[arg(long)]
        wait: bool,
    },
    /// Call a deployed contract
    Call {
        #[command(flatten)]
        signer: Signer,
        #[arg(long)]
        contract: String,
        /// Input bytes as hex
        #[arg(long, conflicts_with = "u64")]
        input: Option<String>,
        /// Input as a little-endian u64 (e.g. the counter increment)
        #[arg(long)]
        u64: Option<u64>,
        #[arg(long, default_value_t = chain::CALL_GAS)]
        gas: u64,
        #[arg(long)]
        wait: bool,
    },
    /// Contract storage (key/value hex map)
    Storage {
        contract: String,
        /// Include not-yet-final blocks
        #[arg(long)]
        pending: bool,
    },
    /// Transaction status and receipt
    Tx { hash: String },
    /// Recent blocks: proposer, QC signers, tx count, finality
    Blocks {
        #[arg(long, default_value_t = 20)]
        last: u64,
    },
}

fn hex0x(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

/// Render byte arrays (as serde emits `[u8; N]`) as 0x-hex strings.
fn hexify(v: Value) -> Value {
    match v {
        Value::Array(items)
            if !items.is_empty() && items.iter().all(|x| x.as_u64().is_some_and(|n| n < 256)) =>
        {
            let bytes: Vec<u8> = items
                .iter()
                .filter_map(|x| x.as_u64())
                .map(|n| n as u8)
                .collect();
            json!(hex0x(&bytes))
        }
        Value::Array(items) => Value::Array(items.into_iter().map(hexify).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, hexify(v))).collect()),
        other => other,
    }
}

fn resolve_address(s: &str) -> Result<Address> {
    if let Some(i) = s.strip_prefix("dev:") {
        let i: u32 = i.parse().context("dev:<index>")?;
        return Ok(chain::address_of(&Keypair::devnet(i)));
    }
    parse_address(s)
}

async fn finish(client: &AetherClient, hash: aether_types::H256, wait: bool) -> Result<Value> {
    if !wait {
        return Ok(json!({ "txHash": hex0x(hash.as_bytes()), "status": "submitted" }));
    }
    let status = client
        .wait_for_finality(hash, Duration::from_secs(60))
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let mut out = json!({ "txHash": hex0x(hash.as_bytes()) });
    if let (Some(o), Some(s)) = (out.as_object_mut(), status.as_object()) {
        for (k, v) in s {
            o.insert(k.clone(), v.clone());
        }
    }
    if status.get("success") == Some(&json!(false)) {
        println!("{}", serde_json::to_string_pretty(&out)?);
        bail!("transaction failed");
    }
    Ok(out)
}

impl ChainArgs {
    pub async fn execute(&self) -> Result<()> {
        let client = AetherClient::new(self.rpc.clone());
        let rpc = |e: aether_sdk::AetherSdkError| anyhow!("{e}");
        let out: Value = match &self.command {
            ChainCommand::Info => client
                .call_rpc("aeth_getNodeInfo", vec![])
                .await
                .map_err(rpc)?,
            ChainCommand::Account { address } => {
                let addr = hex0x(resolve_address(address)?.as_bytes());
                let finalized = client
                    .call_rpc("aeth_getAccount", vec![json!(addr)])
                    .await
                    .map_err(rpc)?;
                let pending = client
                    .call_rpc("aeth_getPendingAccount", vec![json!(addr)])
                    .await
                    .map_err(rpc)?;
                json!({ "address": addr, "finalized": hexify(finalized), "pending": hexify(pending) })
            }
            ChainCommand::Transfer {
                signer,
                to,
                amount,
                wait,
            } => {
                let kp = signer.keypair()?;
                let chain_id = client.chain_id().await.map_err(rpc)?;
                let nonce = client.next_nonce(&kp).await.map_err(rpc)?;
                let fees = client.fee_parameters().await.map_err(rpc)?;
                let tx = chain::transfer_tx_with_fee_parameters(
                    &kp,
                    chain_id,
                    nonce,
                    resolve_address(to)?,
                    *amount,
                    &fees,
                )
                .map_err(rpc)?;
                let hash = client.send_transaction(&tx).await.map_err(rpc)?;
                finish(&client, hash, *wait).await?
            }
            ChainCommand::Deploy { signer, path, wait } => {
                let raw = std::fs::read(path)
                    .with_context(|| format!("failed to read {}", path.display()))?;
                let code = if path.extension().and_then(|e| e.to_str()) == Some("wat") {
                    wat::parse_bytes(&raw)
                        .map_err(|e| anyhow!("invalid WAT: {e}"))?
                        .into_owned()
                } else {
                    raw
                };
                let kp = signer.keypair()?;
                let chain_id = client.chain_id().await.map_err(rpc)?;
                let nonce = client.next_nonce(&kp).await.map_err(rpc)?;
                let fees = client.fee_parameters().await.map_err(rpc)?;
                let (tx, address) =
                    chain::deploy_tx_with_fee_parameters(&kp, chain_id, nonce, code, &fees)
                        .map_err(rpc)?;
                let hash = client.send_transaction(&tx).await.map_err(rpc)?;
                let mut out = finish(&client, hash, *wait).await?;
                out["contract"] = json!(hex0x(address.as_bytes()));
                out
            }
            ChainCommand::Call {
                signer,
                contract,
                input,
                u64,
                gas,
                wait,
            } => {
                let data = match (input, u64) {
                    (Some(h), _) => hex::decode(h.trim_start_matches("0x"))?,
                    (None, Some(v)) => v.to_le_bytes().to_vec(),
                    (None, None) => vec![],
                };
                let kp = signer.keypair()?;
                let chain_id = client.chain_id().await.map_err(rpc)?;
                let nonce = client.next_nonce(&kp).await.map_err(rpc)?;
                let fees = client.fee_parameters().await.map_err(rpc)?;
                let tx = chain::call_tx_with_fee_parameters(
                    &kp,
                    chain_id,
                    nonce,
                    resolve_address(contract)?,
                    data,
                    *gas,
                    &fees,
                )
                .map_err(rpc)?;
                let hash = client.send_transaction(&tx).await.map_err(rpc)?;
                finish(&client, hash, *wait).await?
            }
            ChainCommand::Storage { contract, pending } => client
                .contract_storage(resolve_address(contract)?, *pending)
                .await
                .map_err(rpc)?,
            ChainCommand::Tx { hash } => client
                .transaction_status(parse_h256(hash)?)
                .await
                .map_err(rpc)?,
            ChainCommand::Blocks { last } => {
                let info = client
                    .call_rpc("aeth_getNodeInfo", vec![])
                    .await
                    .map_err(rpc)?;
                let head = info
                    .get("currentSlot")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let from = head.saturating_sub(*last);
                let blocks = client
                    .call_rpc("aeth_getBlockSummaries", vec![json!(from), json!(head)])
                    .await
                    .map_err(rpc)?;
                for b in blocks.as_array().cloned().unwrap_or_default() {
                    let signers = b["parentQc"]["signers"]
                        .as_array()
                        .map(|s| s.len())
                        .unwrap_or(0);
                    let latency = match (b["finalizedAtMs"].as_u64(), b["seenAtMs"].as_u64()) {
                        (Some(f), Some(s)) => format!("{} ms", f.saturating_sub(s)),
                        _ => "-".into(),
                    };
                    println!(
                        "slot {:>6}  {}  proposer {}  txs {:>4}  parent-QC {}/4  {}  seen->final {}",
                        b["slot"],
                        &b["hash"].as_str().unwrap_or("")[..10.min(b["hash"].as_str().unwrap_or("").len())],
                        &b["proposer"].as_str().unwrap_or("")[..10.min(b["proposer"].as_str().unwrap_or("").len())],
                        b["txCount"],
                        signers,
                        if b["finalized"] == json!(true) { "FINAL  " } else { "pending" },
                        latency,
                    );
                }
                return Ok(());
            }
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        Ok(())
    }
}
