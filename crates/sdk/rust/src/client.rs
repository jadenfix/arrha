use aether_types::{Address, Transaction, H256};
use anyhow::Context;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::agent_builder::{
    AgentAuthorizationBuilder, PaymentEnvelopeBuilder, SignatureEnvelopeBuilder, StepReceiptBuilder,
};
use crate::error::AetherSdkError;
use crate::job_builder::JobBuilder;
use crate::transaction_builder::TransferBuilder;
use crate::types::{
    ClientConfig, JobRequest, JobSubmission, NodeHealth, RpcAccount, RpcBlock, RpcReceipt,
    SubmitResponse,
};

#[derive(Clone, Debug)]
pub struct AetherClient {
    endpoint: String,
    config: ClientConfig,
}

impl AetherClient {
    /// Create a new client connected to the given RPC endpoint URL.
    pub fn new(endpoint: impl Into<String>) -> Self {
        AetherClient {
            endpoint: endpoint.into(),
            config: ClientConfig::default(),
        }
    }

    /// Create a new client with a custom configuration.
    pub fn with_config(endpoint: impl Into<String>, config: ClientConfig) -> Self {
        AetherClient {
            endpoint: endpoint.into(),
            config,
        }
    }

    /// Return the RPC endpoint URL.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Return a reference to the client configuration.
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Start building a token transfer transaction.
    pub fn transfer(&self) -> TransferBuilder {
        TransferBuilder::new(&self.config)
    }

    /// Start building an AI job submission.
    pub fn job(&self) -> JobBuilder {
        JobBuilder::new(&self.endpoint)
    }

    /// Start building a typed signature envelope for agent settlement objects.
    pub fn signature_envelope(&self) -> SignatureEnvelopeBuilder {
        SignatureEnvelopeBuilder::new()
    }

    /// Start building an agent session authorization.
    pub fn agent_authorization(&self) -> AgentAuthorizationBuilder {
        AgentAuthorizationBuilder::new()
    }

    /// Start building an agent step receipt.
    pub fn step_receipt(&self) -> StepReceiptBuilder {
        StepReceiptBuilder::new()
    }

    /// Start building an AIC/SWR payment envelope.
    pub fn payment_envelope(&self) -> PaymentEnvelopeBuilder {
        PaymentEnvelopeBuilder::new()
    }

    /// Submit a signed transaction to the network.
    ///
    /// Returns a typed [`AetherSdkError`] so callers can match on specific
    /// failure modes (invalid signature, network I/O, RPC error, hash
    /// mismatch, …) without string inspection.
    pub async fn submit(&self, tx: Transaction) -> Result<SubmitResponse, AetherSdkError> {
        tx.verify_signature()
            .map_err(|e| AetherSdkError::InvalidSignature(e.to_string()))?;
        let fee_params = aether_types::ChainConfig::devnet().fees;
        tx.calculate_fee(&fee_params)
            .map_err(|e| AetherSdkError::InvalidFee(e.to_string()))?;

        let tx_hash = tx.hash();
        let payload = SubmitRpcRequest::new(tx).map_err(AetherSdkError::serialization)?;
        let endpoint = HttpEndpoint::parse(&self.endpoint)?;
        let body = serde_json::to_vec(&payload).map_err(AetherSdkError::serialization)?;
        let headers = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            endpoint.path,
            endpoint.host_header(),
            body.len()
        );

        let response_text = self.rpc_request(&endpoint, &headers, &body).await?;
        let (status_line, rpc_body) = parse_http_response(&response_text)?;
        if !status_line.contains(" 200 ") {
            return Err(AetherSdkError::invalid_response(format!(
                "rpc returned non-success status: {status_line}"
            )));
        }

        let response: JsonRpcResponse<String> = serde_json::from_str(rpc_body).map_err(|e| {
            AetherSdkError::invalid_response(format!("failed to decode rpc response body: {e}"))
        })?;

        if let Some(error) = response.error {
            return Err(AetherSdkError::Rpc {
                code: error.code,
                message: error.message,
            });
        }

        let result_hash = response
            .result
            .ok_or_else(|| AetherSdkError::invalid_response("rpc response missing result"))?;
        let returned_hash = parse_h256_hex(&result_hash)?;

        if returned_hash != tx_hash {
            return Err(AetherSdkError::TxHashMismatch {
                expected: format!("{tx_hash:?}"),
                got: format!("{returned_hash:?}"),
            });
        }

        Ok(SubmitResponse {
            tx_hash: returned_hash,
            accepted: true,
        })
    }

    /// Send a raw HTTP/1.1 JSON-RPC request and return the response body.
    ///
    /// All three I/O phases — TCP connect, write, and response read — are
    /// wrapped in `tokio::time::timeout` using
    /// [`ClientConfig::request_timeout_secs`].  A stalled or
    /// silently-dropped connection cannot therefore block a tokio task
    /// indefinitely in any phase.
    ///
    /// This is the single place where all network I/O happens in the SDK.
    /// Every public method that needs to talk to the RPC endpoint should go
    /// through here so timeout enforcement is consistent.
    async fn rpc_request(
        &self,
        endpoint: &HttpEndpoint,
        headers: &str,
        body: &[u8],
    ) -> Result<String, AetherSdkError> {
        let timeout_dur = Duration::from_secs(self.config.request_timeout_secs);

        // Concatenate headers + body in one buffer to avoid partial-read
        // issues on simple HTTP servers that do a single recv() call.
        let mut payload = Vec::with_capacity(headers.len() + body.len());
        payload.extend_from_slice(headers.as_bytes());
        payload.extend_from_slice(body);

        // Phase 1: TCP connect (timeout-guarded).
        let mut stream = tokio::time::timeout(
            timeout_dur,
            TcpStream::connect((endpoint.host.as_str(), endpoint.port)),
        )
        .await
        .map_err(|_| {
            AetherSdkError::Timeout(format!(
                "timed out connecting to {} after {}s",
                self.endpoint, self.config.request_timeout_secs
            ))
        })?
        .map_err(|e| {
            AetherSdkError::network(format!("failed to connect to {}: {e}", self.endpoint))
        })?;

        // Phase 2: write request (timeout-guarded).
        tokio::time::timeout(timeout_dur, stream.write_all(&payload))
            .await
            .map_err(|_| {
                AetherSdkError::Timeout(format!(
                    "timed out writing rpc request to {} after {}s",
                    self.endpoint, self.config.request_timeout_secs
                ))
            })?
            .map_err(|e| AetherSdkError::network(format!("failed to write rpc request: {e}")))?;

        // Phase 3: read response (timeout-guarded).
        let mut raw = Vec::new();
        tokio::time::timeout(timeout_dur, stream.read_to_end(&mut raw))
            .await
            .map_err(|_| {
                AetherSdkError::Timeout(format!(
                    "timed out reading rpc response from {} after {}s",
                    self.endpoint, self.config.request_timeout_secs
                ))
            })?
            .map_err(|e| AetherSdkError::network(format!("failed to read rpc response: {e}")))?;

        String::from_utf8(raw)
            .map_err(|_| AetherSdkError::invalid_response("rpc response was not valid utf-8"))
    }

    /// Prepare a job submission payload without sending it.
    pub fn prepare_job_submission(&self, job: JobRequest) -> JobSubmission {
        JobSubmission {
            url: format!("{}/v1/jobs", self.endpoint),
            method: "POST".to_string(),
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: job,
        }
    }

    // ──────────────────────────────────────────────────────────────────────────
    // RPC query methods
    // ──────────────────────────────────────────────────────────────────────────

    /// Return the latest slot number (`aeth_getSlotNumber`).
    pub async fn get_block_number(&self) -> Result<u64, AetherSdkError> {
        let result: Value = self.rpc_call("aeth_getSlotNumber", &[]).await?;
        result
            .as_u64()
            .ok_or_else(|| AetherSdkError::invalid_response("expected u64 from aeth_getSlotNumber"))
    }

    /// Fetch a block by its hash (`aeth_getBlockByHash`).
    ///
    /// Returns `None` if the block is not found.
    pub async fn get_block_by_hash(
        &self,
        hash: H256,
        full_tx: bool,
    ) -> Result<Option<RpcBlock>, AetherSdkError> {
        let hash_hex = format!("0x{}", hex::encode(hash.as_bytes()));
        let result: Value = self
            .rpc_call(
                "aeth_getBlockByHash",
                &[Value::String(hash_hex), Value::Bool(full_tx)],
            )
            .await?;
        if result.is_null() {
            return Ok(None);
        }
        serde_json::from_value(result)
            .map(Some)
            .map_err(|e| AetherSdkError::invalid_response(format!("failed to decode block: {e}")))
    }

    /// Fetch a block by its slot number (`aeth_getBlockByNumber`).
    ///
    /// Pass `None` for `slot` to fetch the latest block.
    /// Returns `None` if no block exists at that slot.
    pub async fn get_block_by_number(
        &self,
        slot: Option<u64>,
        full_tx: bool,
    ) -> Result<Option<RpcBlock>, AetherSdkError> {
        let slot_ref = slot.map_or_else(|| "latest".to_string(), |n| n.to_string());
        let result: Value = self
            .rpc_call(
                "aeth_getBlockByNumber",
                &[Value::String(slot_ref), Value::Bool(full_tx)],
            )
            .await?;
        if result.is_null() {
            return Ok(None);
        }
        serde_json::from_value(result)
            .map(Some)
            .map_err(|e| AetherSdkError::invalid_response(format!("failed to decode block: {e}")))
    }

    /// Fetch a transaction receipt by tx hash (`aeth_getTransactionReceipt`).
    ///
    /// Returns `None` if the transaction is not found.
    pub async fn get_transaction_receipt(
        &self,
        tx_hash: H256,
    ) -> Result<Option<RpcReceipt>, AetherSdkError> {
        let hash_hex = format!("0x{}", hex::encode(tx_hash.as_bytes()));
        let result: Value = self
            .rpc_call("aeth_getTransactionReceipt", &[Value::String(hash_hex)])
            .await?;
        if result.is_null() {
            return Ok(None);
        }
        serde_json::from_value(result)
            .map(Some)
            .map_err(|e| AetherSdkError::invalid_response(format!("failed to decode receipt: {e}")))
    }

    /// Fetch the account state for the given address (`aeth_getAccount`).
    pub async fn get_account(
        &self,
        address: Address,
    ) -> Result<Option<RpcAccount>, AetherSdkError> {
        let addr_hex = format!("0x{}", hex::encode(address.as_bytes()));
        let result: Value = self
            .rpc_call("aeth_getAccount", &[Value::String(addr_hex)])
            .await?;
        if result.is_null() {
            return Ok(None);
        }
        serde_json::from_value(result)
            .map(Some)
            .map_err(|e| AetherSdkError::invalid_response(format!("failed to decode account: {e}")))
    }

    /// Fetch the current state root (`aeth_getStateRoot`).
    ///
    /// Returns the root of the sparse Merkle tree over all accounts.
    pub async fn get_state_root(&self) -> Result<H256, AetherSdkError> {
        let result: Value = self.rpc_call("aeth_getStateRoot", &[]).await?;
        let hex_str = result.as_str().ok_or_else(|| {
            AetherSdkError::invalid_response("expected hex string from aeth_getStateRoot")
        })?;
        parse_h256_hex(hex_str)
    }

    /// Fetch the node health status (`aeth_health`).
    ///
    /// `status` is `"ok"` when fully synced or `"syncing"` when catching up.
    pub async fn get_health(&self) -> Result<NodeHealth, AetherSdkError> {
        let result: Value = self.rpc_call("aeth_health", &[]).await?;
        serde_json::from_value(result)
            .map_err(|e| AetherSdkError::invalid_response(format!("failed to decode health: {e}")))
    }

    /// Send a JSON-RPC request and return the `result` field as a `Value`.
    ///
    /// All I/O is routed through [`rpc_request`] so every method
    /// automatically inherits the connect + write + read timeout enforced
    /// there.  A stalled server cannot block any query method indefinitely.
    ///
    /// On JSON-RPC error, maps to `AetherSdkError::Rpc`.
    pub(crate) async fn rpc_call(
        &self,
        method: &str,
        params: &[Value],
    ) -> Result<Value, AetherSdkError> {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": 1,
        });
        let body = serde_json::to_vec(&payload).map_err(AetherSdkError::serialization)?;
        let endpoint = HttpEndpoint::parse(&self.endpoint)?;
        let headers = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            endpoint.path,
            endpoint.host_header(),
            body.len()
        );

        let response_text = self.rpc_request(&endpoint, &headers, &body).await?;
        let (status_line, rpc_body) = parse_http_response(&response_text)?;
        if !status_line.contains(" 200 ") {
            return Err(AetherSdkError::invalid_response(format!(
                "rpc returned non-success status: {status_line}"
            )));
        }

        let response: JsonRpcResponse<Value> = serde_json::from_str(rpc_body).map_err(|e| {
            AetherSdkError::invalid_response(format!("failed to decode rpc response: {e}"))
        })?;

        if let Some(error) = response.error {
            return Err(AetherSdkError::Rpc {
                code: error.code,
                message: error.message,
            });
        }

        response
            .result
            .ok_or_else(|| AetherSdkError::invalid_response("rpc response missing result field"))
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct SubmitRpcRequest {
    jsonrpc: &'static str,
    method: &'static str,
    params: Vec<String>,
    id: u64,
}

impl SubmitRpcRequest {
    fn new(tx: Transaction) -> anyhow::Result<Self> {
        let bytes = bincode::serialize(&tx).context("failed to serialize transaction")?;
        Ok(Self {
            jsonrpc: "2.0",
            method: "aeth_sendRawTransaction",
            params: vec![format!("0x{}", hex::encode(bytes))],
            id: 1,
        })
    }
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponse<T> {
    result: Option<T>,
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Debug)]
struct HttpEndpoint {
    host: String,
    port: u16,
    path: String,
}

impl HttpEndpoint {
    fn parse(endpoint: &str) -> Result<Self, AetherSdkError> {
        let trimmed = endpoint.trim();
        let without_scheme = trimmed.strip_prefix("http://").ok_or_else(|| {
            AetherSdkError::invalid_endpoint(format!(
                "only http:// endpoints are supported, got: {trimmed}"
            ))
        })?;

        let (host_port, path) = if let Some((h, p)) = without_scheme.split_once('/') {
            (h, format!("/{}", p))
        } else {
            (without_scheme, "/".to_string())
        };

        if host_port.is_empty() {
            return Err(AetherSdkError::invalid_endpoint(format!(
                "invalid endpoint host: {endpoint}"
            )));
        }

        let (host, port) = if let Some((h, p)) = host_port.rsplit_once(':') {
            let parsed_port = p.parse::<u16>().map_err(|_| {
                AetherSdkError::invalid_endpoint(format!("invalid endpoint port in {endpoint}"))
            })?;
            (h.to_string(), parsed_port)
        } else {
            (host_port.to_string(), 80)
        };

        if host.is_empty() {
            return Err(AetherSdkError::invalid_endpoint(format!(
                "invalid endpoint host: {endpoint}"
            )));
        }

        Ok(Self { host, port, path })
    }

    fn host_header(&self) -> String {
        if self.port == 80 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn parse_http_response(response: &str) -> Result<(&str, &str), AetherSdkError> {
    let (headers, body) = response.split_once("\r\n\r\n").ok_or_else(|| {
        AetherSdkError::invalid_response("invalid http response from rpc endpoint")
    })?;
    let status_line = headers
        .lines()
        .next()
        .ok_or_else(|| AetherSdkError::invalid_response("missing http status line"))?;
    Ok((status_line, body))
}

fn parse_h256_hex(value: &str) -> Result<aether_types::H256, AetherSdkError> {
    let bytes = hex::decode(value.trim_start_matches("0x"))
        .map_err(|_| AetherSdkError::invalid_response(format!("invalid tx hash hex: {value}")))?;
    aether_types::H256::from_slice(&bytes)
        .map_err(|e| AetherSdkError::invalid_response(format!("invalid tx hash: {e}")))
}
