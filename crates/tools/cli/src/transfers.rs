use std::path::PathBuf;

use aether_types::Address;
use anyhow::{anyhow, Result};
use clap::Args;
use serde::Serialize;

use crate::config::{expand_path, ResolvedConfig};
use crate::io::{address_to_string, h256_to_string, parse_address, read_key_file};

#[derive(Args, Debug)]
pub struct TransferCommand {
    /// Destination address (hex-encoded, 0x-prefixed)
    #[arg(long)]
    pub to: String,

    /// Amount to transfer (in base units)
    #[arg(long)]
    pub amount: u128,

    /// Sender nonce for the transaction
    #[arg(long)]
    pub nonce: u64,

    /// Optional memo field recorded with the transfer
    #[arg(long)]
    pub memo: Option<String>,

    /// Override default fee
    #[arg(long, value_name = "FEE")]
    pub fee: Option<u128>,

    /// Override default gas limit
    #[arg(long, value_name = "GAS")]
    pub gas_limit: Option<u64>,

    /// Path to the signing key; defaults to config default_key
    #[arg(long, value_name = "PATH")]
    pub key: Option<String>,
}

impl TransferCommand {
    pub async fn execute(&self, config: &ResolvedConfig) -> Result<()> {
        let recipient = parse_address(&self.to)?;
        let params = TransferParams {
            recipient,
            amount: self.amount,
            nonce: self.nonce,
            memo: self.memo.clone(),
            fee: self.fee,
            gas_limit: self.gas_limit,
        };
        let summary = perform_transfer(config, self.key.as_deref(), params).await?;

        println!("{}", serde_json::to_string_pretty(&summary)?);
        Ok(())
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct TransferSummary {
    pub tx_hash: String,
    pub accepted: bool,
    pub sender: String,
    pub recipient: String,
    pub amount: u128,
    pub fee: u128,
    pub gas_limit: u64,
    pub memo: Option<String>,
    pub endpoint: String,
}

pub struct TransferParams {
    pub recipient: Address,
    pub amount: u128,
    pub nonce: u64,
    pub memo: Option<String>,
    pub fee: Option<u128>,
    pub gas_limit: Option<u64>,
}

pub async fn perform_transfer(
    config: &ResolvedConfig,
    key_path: Option<&str>,
    params: TransferParams,
) -> Result<TransferSummary> {
    let key_path = resolve_key_path(config, key_path)?;
    let key_material = read_key_file(&key_path)?;

    let client = config.client();
    let mut builder = client
        .transfer()
        .chain_id(client.get_chain_id().await?)
        .to(params.recipient)
        .amount(params.amount);
    if let Some(memo) = &params.memo {
        builder = builder.memo(memo.clone());
    }
    if let Some(fee) = params.fee {
        builder = builder.fee(fee);
    }
    if let Some(gas) = params.gas_limit {
        builder = builder.gas_limit(gas);
    }

    let tx = builder.build(&key_material.keypair, params.nonce)?;
    let tx_clone = tx.clone();
    let response = client.submit(tx).await?;

    let summary = TransferSummary {
        tx_hash: h256_to_string(&response.tx_hash),
        accepted: response.accepted,
        sender: address_to_string(&tx_clone.sender),
        recipient: address_to_string(&params.recipient),
        amount: params.amount,
        fee: tx_clone.fee,
        gas_limit: tx_clone.gas_limit,
        memo: params.memo,
        endpoint: config.endpoint.clone(),
    };

    Ok(summary)
}

fn resolve_key_path(config: &ResolvedConfig, override_path: Option<&str>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return expand_path(path);
    }
    if let Some(default) = config.default_key_path() {
        return Ok(default.to_path_buf());
    }
    Err(anyhow!(
        "no signing key provided; pass --key PATH or set default_key in config"
    ))
}
