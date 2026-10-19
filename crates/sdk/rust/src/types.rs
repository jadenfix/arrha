use serde::{Deserialize, Serialize};
use serde_json::Value;

use aether_types::{Address, Block, H256};

/// A block as returned by `aeth_getBlockByHash` / `aeth_getBlockByNumber`.
/// The RPC response preserves the protocol's nested header structure.
pub type RpcBlock = Block;

/// Transaction receipt as returned by `aeth_getTransactionReceipt`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcReceipt {
    pub tx_hash: H256,
    pub block_hash: H256,
    pub slot: u64,
    pub status: Value,
    pub gas_used: u64,
}

/// Account state as returned by `aeth_getAccount`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcAccount {
    pub address: Address,
    pub balance: u128,
    pub nonce: u64,
    #[serde(default)]
    pub stake: u128,
}

/// Node health response from `aeth_health`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeHealth {
    /// `"ok"` when fully synced, `"syncing"` when catching up.
    pub status: String,
    pub version: String,
    #[serde(rename = "latestSlot")]
    pub latest_slot: u64,
    #[serde(rename = "finalizedSlot")]
    pub finalized_slot: u64,
    #[serde(rename = "peerCount")]
    pub peer_count: usize,
    pub sync: Value,
}

fn default_timeout_secs() -> u64 {
    30
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientConfig {
    pub default_fee: u128,
    pub default_gas_limit: u64,
    /// Timeout in seconds for each RPC request (TCP connect + read).
    ///
    /// Defaults to 30 seconds. Set to a lower value for latency-sensitive
    /// contexts (e.g. health checks) or a higher value for slow networks.
    #[serde(default = "default_timeout_secs")]
    pub request_timeout_secs: u64,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            default_fee: 2_000_000,
            default_gas_limit: 500_000,
            request_timeout_secs: default_timeout_secs(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransferRequest {
    pub recipient: Address,
    pub amount: u128,
    pub memo: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubmitResponse {
    pub tx_hash: H256,
    pub accepted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobRequest {
    pub job_id: String,
    pub model_hash: H256,
    pub input_hash: H256,
    pub max_fee: u128,
    pub expires_at: u64,
    pub metadata: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobSubmission {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: JobRequest,
}
