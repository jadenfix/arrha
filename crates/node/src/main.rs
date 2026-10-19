use std::env;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use aether_ledger::{ContractCall, ContractExecutor, ContractOutcome};
use aether_metrics::exporter::start_metrics_exporter;
use aether_node::SyncRequest;
use aether_node::{
    create_hybrid_consensus, create_hybrid_consensus_with_all_keys, encode_tx_batch, unix_millis,
    validator_info_from_keypair, GenesisConfig, Node, OutboundMessage, ValidatorKeypair,
};
use aether_p2p::network::{P2PNetwork, TOPIC_SYNC, TOPIC_TX, TOPIC_VOTE};
use aether_rpc_json::{JsonRpcServer, RpcBackend};
use aether_runtime::{ExecutionContext, WasmVm};
use aether_types::{Address, Block, ChainConfig, Transaction, TransactionReceipt, H256};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};

/// Maximum fuel (gas) a single contract call may use.
const MAX_CONTRACT_GAS: u64 = 10_000_000;

/// Contract runtime backed by the wasmtime VM in `aether-runtime`.
struct WasmExecutor {
    vm: std::sync::Mutex<WasmVm>,
}

impl ContractExecutor for WasmExecutor {
    fn validate(&self, code: &[u8]) -> Result<()> {
        self.vm
            .lock()
            .map_err(|_| anyhow::anyhow!("vm lock poisoned"))?
            .validate_module(code)
    }

    fn execute(&self, call: ContractCall<'_>) -> Result<ContractOutcome> {
        let ctx = ExecutionContext {
            contract_address: call.contract,
            caller: call.caller,
            value: 0,
            gas_limit: call.gas_limit.min(MAX_CONTRACT_GAS),
            block_number: call.context.slot,
            timestamp: call.context.timestamp,
        };
        let result = self
            .vm
            .lock()
            .map_err(|_| anyhow::anyhow!("vm lock poisoned"))?
            .execute_with_storage(call.code, &ctx, call.input, call.storage)?;
        Ok(ContractOutcome {
            success: result.success,
            gas_used: result.gas_used,
            storage: result.storage_changes,
            logs: result.logs.into_iter().map(|l| l.data).collect(),
            return_data: result.return_data,
        })
    }
}

struct NodeRpcBackend {
    genesis_hash: H256,
    chain_identity: H256,
    node: Arc<RwLock<Node>>,
    airdrop_enabled: bool,
}

fn hex0x(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

fn parse_h256(v: Option<&Value>) -> Result<H256> {
    let s = v
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("expected 0x-prefixed hash"))?;
    let bytes = hex::decode(s.trim_start_matches("0x"))?;
    H256::from_slice(&bytes).map_err(|e| anyhow::anyhow!("{e}"))
}

fn parse_address(v: Option<&Value>) -> Result<Address> {
    let s = v
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("expected 0x-prefixed address"))?;
    let bytes = hex::decode(s.trim_start_matches("0x"))?;
    Address::from_slice(&bytes).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Compact JSON summary of a block for explorers and benchmark tooling.
fn block_summary(node: &Node, block: &Block, with_txs: bool) -> Value {
    let hash = block.hash();
    let times = node.block_times(&hash).unwrap_or_default();
    let finalized = node.is_finalized(&hash, block.header.slot);
    let qc = block.aggregated_vote.as_ref().map(|q| {
        json!({
            "slot": q.slot,
            "blockHash": hex0x(q.block_hash.as_bytes()),
            "signers": q.signers.iter().map(|p| hex0x(p.to_address().as_bytes())).collect::<Vec<_>>(),
        })
    });
    let mut out = json!({
        "slot": block.header.slot,
        "hash": hex0x(hash.as_bytes()),
        "parentHash": hex0x(block.header.parent_hash.as_bytes()),
        "proposer": hex0x(block.header.proposer.as_bytes()),
        "timestamp": block.header.timestamp,
        "stateRoot": hex0x(block.header.state_root.as_bytes()),
        "txCount": block.transactions.len(),
        "parentQc": qc,
        "finalized": finalized,
        "seenAtMs": times.seen_ms,
        "finalizedAtMs": times.finalized_ms,
    });
    if with_txs {
        out["txHashes"] = json!(block
            .transactions
            .iter()
            .map(|tx| hex0x(tx.hash().as_bytes()))
            .collect::<Vec<_>>());
    }
    out
}

impl NodeRpcBackend {
    fn read_node(&self) -> Result<std::sync::RwLockReadGuard<'_, Node>> {
        self.node
            .read()
            .map_err(|_| anyhow::anyhow!("node lock poisoned"))
    }

    fn write_node(&self) -> Result<std::sync::RwLockWriteGuard<'_, Node>> {
        self.node
            .write()
            .map_err(|_| anyhow::anyhow!("node lock poisoned"))
    }
}

impl RpcBackend for NodeRpcBackend {
    fn send_raw_transaction(&self, tx_bytes: Vec<u8>) -> Result<H256> {
        let tx: Transaction =
            bincode::deserialize(&tx_bytes).context("failed to decode transaction bytes")?;
        let mut node = self.write_node()?;
        node.submit_transaction(tx)
    }

    fn get_block_by_number(&self, block_number: u64, _full_tx: bool) -> Result<Option<Block>> {
        let node = self.read_node()?;
        Ok(node.get_block_by_slot(block_number))
    }

    fn get_block_by_hash(&self, block_hash: H256, _full_tx: bool) -> Result<Option<Block>> {
        let node = self.read_node()?;
        Ok(node.get_block_by_hash(block_hash))
    }

    fn get_transaction_receipt(&self, tx_hash: H256) -> Result<Option<TransactionReceipt>> {
        let node = self.read_node()?;
        Ok(node.get_transaction_receipt(tx_hash))
    }

    fn get_state_root(&self, _block_ref: Option<String>) -> Result<H256> {
        let node = self.read_node()?;
        Ok(node.get_state_root())
    }

    fn get_account(&self, address: Address, _block_ref: Option<String>) -> Result<Option<Value>> {
        let node = self.read_node()?;
        match node.get_account(address)? {
            Some(account) => Ok(Some(serde_json::to_value(account)?)),
            None => Ok(None),
        }
    }

    fn get_slot_number(&self) -> Result<u64> {
        let node = self.read_node()?;
        Ok(node.current_slot())
    }

    fn get_finalized_slot(&self) -> Result<u64> {
        let node = self.read_node()?;
        Ok(node.finalized_slot())
    }

    fn get_latest_block_slot(&self) -> Result<Option<u64>> {
        let node = self.read_node()?;
        Ok(node.latest_block_slot())
    }

    fn get_peer_count(&self) -> Result<usize> {
        let node = self.read_node()?;
        Ok(node.peer_count())
    }

    fn get_sync_status(&self) -> Result<Value> {
        let node = self.read_node()?;
        let syncing = node.is_syncing();
        let current_slot = node.current_slot();
        let finalized = node.finalized_slot();
        Ok(json!({
            "syncing": syncing,
            "currentSlot": current_slot,
            "finalizedSlot": finalized,
        }))
    }

    fn allows_airdrop(&self) -> bool {
        // Seeding balances outside of blocks only works with one validator;
        // in a multi-validator network it would fork the state.
        self.airdrop_enabled
            && self
                .read_node()
                .map(|node| node.allows_airdrop())
                .unwrap_or(false)
    }

    fn request_airdrop(&self, address: Address, amount: u128) -> Result<()> {
        let mut node = self.write_node()?;
        node.seed_account(&address, amount)
    }

    fn extension(&self, method: &str, params: &[Value]) -> Option<Result<Value>> {
        let result = match method {
            // Submit many bincode transactions (hex strings) in one request.
            "aeth_sendRawTransactions" => (|| {
                let list = params
                    .first()
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| anyhow::anyhow!("expected an array of hex transactions"))?;
                let mut txs = Vec::with_capacity(list.len());
                let mut results = vec![Value::Null; list.len()];
                let mut index = Vec::with_capacity(list.len());
                for (i, item) in list.iter().enumerate() {
                    let decoded = item
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("not a string"))
                        .and_then(|s| Ok(hex::decode(s.trim_start_matches("0x"))?))
                        .and_then(|b| Ok(bincode::deserialize::<Transaction>(&b)?));
                    match decoded {
                        Ok(tx) => {
                            txs.push(tx);
                            index.push(i);
                        }
                        Err(e) => results[i] = json!({ "error": e.to_string() }),
                    }
                }
                let mut node = self.write_node()?;
                for (i, r) in index.into_iter().zip(node.submit_transactions(txs)) {
                    results[i] = match r {
                        Ok(h) => json!(hex0x(h.as_bytes())),
                        Err(e) => json!({ "error": e.to_string() }),
                    };
                }
                Ok(json!(results))
            })(),
            // Block summaries for a slot range (finalized + pending head chain).
            "aeth_getBlockSummaries" => (|| {
                let from = params.first().and_then(|v| v.as_u64()).unwrap_or(0);
                let to = params.get(1).and_then(|v| v.as_u64()).unwrap_or(from);
                let with_txs = params.get(2).and_then(|v| v.as_bool()).unwrap_or(false);
                if to < from || to - from > 1000 {
                    anyhow::bail!("range must be 0..=1000 slots");
                }
                let node = self.read_node()?;
                let out: Vec<Value> = (from..=to)
                    .filter_map(|slot| node.get_block_by_slot(slot))
                    .map(|b| block_summary(&node, &b, with_txs))
                    .collect();
                Ok(json!(out))
            })(),
            "aeth_getTransactionStatus" => (|| {
                let hash = parse_h256(params.first())?;
                let node = self.read_node()?;
                Ok(match node.get_transaction_receipt(hash) {
                    None => json!({ "status": "unknown" }),
                    Some(r) => {
                        let finalized = node.is_finalized(&r.block_hash, r.slot);
                        let ok = matches!(r.status, aether_types::TransactionStatus::Success);
                        json!({
                            "status": if finalized { "finalized" } else { "included" },
                            "slot": r.slot,
                            "blockHash": hex0x(r.block_hash.as_bytes()),
                            "success": ok,
                            "error": match &r.status {
                                aether_types::TransactionStatus::Failed { reason } => json!(reason),
                                _ => Value::Null,
                            },
                            "gasUsed": r.gas_used,
                            "logs": r.logs.iter().map(|l| hex0x(&l.data)).collect::<Vec<_>>(),
                        })
                    }
                })
            })(),
            "aeth_getContractStorage" => (|| {
                let address = parse_address(params.first())?;
                let pending = params.get(1).and_then(|v| v.as_str()) == Some("pending");
                let node = self.read_node()?;
                let storage = node.get_contract_storage(address, pending)?;
                let map: serde_json::Map<String, Value> = storage
                    .iter()
                    .map(|(k, v)| (hex0x(k), json!(hex0x(v))))
                    .collect();
                Ok(Value::Object(map))
            })(),
            "aeth_getPendingAccount" => (|| {
                let address = parse_address(params.first())?;
                let node = self.read_node()?;
                Ok(serde_json::to_value(node.get_account_pending(address)?)?)
            })(),
            "aeth_getFeeParameters" => (|| {
                let node = self.read_node()?;
                let config = node.chain_config();
                Ok(json!({
                    "a": config.fees.a.to_string(),
                    "b": config.fees.b.to_string(),
                    "c": config.fees.c.to_string(),
                    "minBaseFee": config.fees.min_base_fee.to_string(),
                    "chainId": config.chain.chain_id_numeric,
                }))
            })(),
            "aeth_getNodeInfo" => (|| {
                let node = self.read_node()?;
                let (tip, tip_slot) = node.finalized_tip();
                Ok(json!({
                    "genesisHash": hex0x(self.genesis_hash.as_bytes()),
                    "chainIdentity": hex0x(self.chain_identity.as_bytes()),
                    "chainId": node.chain_config().chain.chain_id_numeric,
                    "protocolVersion": aether_types::PROTOCOL_VERSION,
                    "validator": node.validator_address().map(|a| hex0x(a.as_bytes())),
                    "currentSlot": node.current_slot(),
                    "finalizedSlot": tip_slot,
                    "finalizedHash": hex0x(tip.as_bytes()),
                    "latestBlockSlot": node.latest_block_slot(),
                    "pendingBlocks": node.pending_block_count(),
                    "mempool": node.mempool_size(),
                    "peers": node.peer_count(),
                    "orphans": node.orphan_count(),
                }))
            })(),
            _ => return None,
        };
        Some(result)
    }
}

/// Maximum network events to drain per tick. Prevents holding the node lock
/// for an unbounded time when the inbound channel is flooded (e.g., during
/// initial sync or a message flood from peers). Remaining events are
/// processed on the next tick.
const MAX_EVENTS_PER_TICK: usize = 256;

async fn run_slot_loop(
    node: Arc<RwLock<Node>>,
    mut net_rx: mpsc::Receiver<aether_p2p::network::NetworkEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    // Network events are handled as they arrive (votes and blocks must not
    // wait for the next slot), slots fire on wall-clock boundaries, and
    // locally submitted transactions are gossiped in batches every 20 ms.
    let mut gossip_tick = tokio::time::interval(Duration::from_millis(20));
    gossip_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if *shutdown_rx.borrow() {
            tracing::info!("Slot loop received shutdown signal, stopping");
            return Ok(());
        }
        // Run the slot logic as soon as a boundary has passed. Checking here
        // (not only when the sleep below wins the select) matters under load:
        // a steady stream of network events would otherwise keep winning the
        // select and the validator would silently skip its leader slots.
        {
            let mut guard = node
                .write()
                .map_err(|_| anyhow::anyhow!("node lock poisoned"))?;
            guard.tick_wallclock(unix_millis())?;
        }
        let now = unix_millis();
        let next_boundary = {
            let guard = node
                .read()
                .map_err(|_| anyhow::anyhow!("node lock poisoned"))?;
            let slot = guard.wallclock_slot(now).unwrap_or(0);
            guard.slot_start_ms(slot + 1).unwrap_or(now + 500)
        };
        let wait_ms = next_boundary.saturating_sub(now).max(1);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(wait_ms)) => {
                let mut guard = node
                    .write()
                    .map_err(|_| anyhow::anyhow!("node lock poisoned"))?;
                guard.tick_wallclock(unix_millis())?;
            }
            event = net_rx.recv() => {
                let Some(event) = event else { return Ok(()) };
                let mut guard = node
                    .write()
                    .map_err(|_| anyhow::anyhow!("node lock poisoned"))?;
                guard.handle_network_event(event)?;
                // Drain whatever else is queued, bounded by count and by the
                // next slot boundary so a burst of blocks/tx batches never
                // makes this validator miss its own leader slot.
                let mut processed = 1usize;
                while processed < MAX_EVENTS_PER_TICK && unix_millis() < next_boundary {
                    match net_rx.try_recv() {
                        Ok(event) => {
                            guard.handle_network_event(event)?;
                            processed += 1;
                        }
                        Err(_) => break,
                    }
                }
            }
            _ = gossip_tick.tick() => {
                let mut guard = node
                    .write()
                    .map_err(|_| anyhow::anyhow!("node lock poisoned"))?;
                guard.flush_tx_gossip();
            }
            _ = shutdown_rx.changed() => {
                tracing::info!("Slot loop received shutdown signal, stopping");
                return Ok(());
            }
        }
    }
}

/// P2P outbound loop: reads OutboundMessages from node and publishes to network.
async fn run_p2p_outbound(
    mut p2p: P2PNetwork,
    mut outbound_rx: mpsc::Receiver<OutboundMessage>,
    net_tx: mpsc::Sender<aether_p2p::network::NetworkEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let mut inbound_event_drops: u64 = 0;

    loop {
        tokio::select! {
            // Check for shutdown signal
            _ = shutdown_rx.changed() => {
                tracing::info!("P2P outbound loop received shutdown signal, stopping");
                return Ok(());
            }
            // Poll for inbound P2P events
            event = p2p.poll() => {
                if let Some(event) = event {
                    if net_tx.try_send(event).is_err() {
                        inbound_event_drops += 1;
                        tracing::warn!(
                            total_drops = inbound_event_drops,
                            "Inbound P2P channel full, dropping network event"
                        );
                    }
                }
            }
            // Handle outbound messages from the node
            msg = outbound_rx.recv() => {
                match msg {
                    Some(OutboundMessage::BroadcastBlock(block)) => {
                        if let Err(e) = p2p.broadcast_block(&block) {
                            tracing::warn!("failed to broadcast block: {e}");
                        }
                    }
                    Some(OutboundMessage::BroadcastVote(vote)) => {
                        match bincode::serialize(&vote) {
                            Ok(data) => {
                                if let Err(e) = p2p.publish(TOPIC_VOTE, data) {
                                    tracing::warn!("failed to broadcast vote: {e}");
                                }
                            }
                            Err(e) => {
                                tracing::error!("failed to serialize vote: {e}");
                            }
                        }
                    }
                    Some(OutboundMessage::BroadcastTransaction(tx)) => {
                        match encode_tx_batch(std::slice::from_ref(&tx)) {
                            Ok(data) => {
                                if let Err(e) = p2p.publish(TOPIC_TX, data) {
                                    tracing::warn!("failed to broadcast tx: {e}");
                                }
                            }
                            Err(e) => tracing::error!("failed to serialize tx: {e}"),
                        }
                    }
                    Some(OutboundMessage::BroadcastTransactions(txs)) => {
                        match encode_tx_batch(&txs) {
                            Ok(data) => {
                                if let Err(e) = p2p.publish(TOPIC_TX, data) {
                                    tracing::warn!(count = txs.len(), "failed to broadcast tx batch: {e}");
                                }
                            }
                            Err(e) => tracing::error!("failed to serialize tx batch: {e}"),
                        }
                    }
                    Some(OutboundMessage::RequestBlockRange { from_slot, to_slot }) => {
                        let req = SyncRequest { from_slot, to_slot };
                        let data = bincode::serialize(&req).unwrap_or_else(|e| {
                            tracing::error!("failed to serialize sync request: {e}");
                            Vec::new()
                        });
                        if !data.is_empty() {
                            if let Err(e) = p2p.publish(TOPIC_SYNC, data) {
                                tracing::warn!("failed to publish sync request: {e}");
                            }
                        }
                    }
                    None => break, // Channel closed
                }
            }
        }
    }
    Ok(())
}

/// Wait for a SIGTERM signal (used for graceful shutdown in containers).
#[cfg(unix)]
async fn sigterm_recv() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sig = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
    sig.recv().await;
}

#[cfg(not(unix))]
async fn sigterm_recv() {
    // SIGTERM is not available on non-Unix platforms; pend forever.
    std::future::pending::<()>().await;
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    tracing::info!("Aether Node v0.3.0");
    tracing::info!("=================\n");

    // Read shared genesis before selecting the runtime configuration. An
    // operator's genesis parameters must not silently become the devnet preset.
    let genesis_file: Option<GenesisConfig> = env::var("AETHER_GENESIS_PATH")
        .ok()
        .map(|path| -> Result<GenesisConfig> {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("failed to read genesis file: {path}"))?;
            let genesis: GenesisConfig =
                serde_json::from_slice(&bytes).context("failed to parse genesis JSON")?;
            genesis.validate()?;
            Ok(genesis)
        })
        .transpose()?;

    // Explicit configuration overrides genesis; otherwise use its parameters.
    let chain_config = if let Ok(config_path) = env::var("AETHER_CONFIG_PATH") {
        tracing::info!("Loading config from: {config_path}");
        ChainConfig::from_toml_file(Path::new(&config_path))?
    } else if let Some(genesis) = genesis_file
        .as_ref()
        .filter(|_| env::var_os("AETHER_NETWORK").is_none())
    {
        genesis.chain_config.clone()
    } else {
        let network = env::var("AETHER_NETWORK").unwrap_or_else(|_| "devnet".to_string());
        tracing::info!("Using {network} preset config");
        match network.as_str() {
            "mainnet" => ChainConfig::mainnet(),
            "testnet" => ChainConfig::testnet(),
            "devnet" => ChainConfig::devnet(),
            other => bail!("unknown AETHER_NETWORK preset: {other}"),
        }
    };

    let chain_config = Arc::new(chain_config);
    tracing::info!(
        "Chain: {} (numeric ID: {})",
        chain_config.chain.chain_id,
        chain_config.chain.chain_id_numeric
    );

    let db_path = env::var("AETHER_NODE_DB_PATH").unwrap_or_else(|_| "./data/node1".to_string());

    // Load or generate validator keypair
    let key_path =
        env::var("AETHER_VALIDATOR_KEY").unwrap_or_else(|_| format!("{}/validator.key", db_path));
    let key_path = std::path::Path::new(&key_path);

    let validator_keypair = if key_path.exists() {
        tracing::info!(path = %key_path.display(), "Loading validator key");
        ValidatorKeypair::load_from_file(key_path)?
    } else {
        tracing::info!("Generating new validator keypair...");
        let kp = ValidatorKeypair::generate();
        // Persist the identity before opening the ledger or participating.
        if let Some(parent) = key_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).context("creating validator key directory")?;
        }
        kp.save_to_file(key_path)
            .context("persisting generated validator identity")?;
        tracing::info!(path = %key_path.display(), "Saved validator key");
        kp
    };
    let validator_address = validator_keypair.address();

    // Build consensus from genesis file (multi-validator) or single-validator mode
    let consensus: Box<dyn aether_consensus::ConsensusEngine> = if let Some(genesis) = &genesis_file
    {
        let result = genesis.build();
        let vrf_pubkeys = genesis.vrf_pubkeys();
        let bls_pubkeys = genesis.bls_pubkeys();

        tracing::info!(
            validators = result.validator_set.len(),
            total_stake = result.total_stake,
            "Genesis config loaded"
        );

        let engine = create_hybrid_consensus_with_all_keys(
            result.validator_set,
            vrf_pubkeys,
            bls_pubkeys,
            Some(&validator_keypair),
            chain_config.consensus.tau,
            chain_config.chain.epoch_slots,
        )?;
        Box::new(engine)
    } else {
        // Single-validator quick-start mode
        let validators = vec![validator_info_from_keypair(&validator_keypair, 1_000_000)];
        Box::new(create_hybrid_consensus(
            validators,
            Some(&validator_keypair),
            chain_config.consensus.tau,
            chain_config.chain.epoch_slots,
        )?)
    };

    let rpc_port: u16 = env::var("AETHER_RPC_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8545);
    let p2p_port: u16 = env::var("AETHER_P2P_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9000);
    let metrics_port: u16 = env::var("AETHER_METRICS_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9090);

    let mut node = Node::new(
        db_path,
        consensus,
        Some(validator_keypair.ed25519),
        Some(validator_keypair.bls),
        chain_config.clone(),
    )?;
    node.set_contract_executor(Arc::new(WasmExecutor {
        vm: std::sync::Mutex::new(WasmVm::new(MAX_CONTRACT_GAS)?),
    }));
    if let Ok(path) = env::var("AETHER_EVENT_LOG") {
        node.set_event_log(Path::new(&path))?;
        tracing::info!(path, "Writing consensus event log");
    }

    // Genesis state: every validator seeds the SAME balances from the shared
    // genesis file (previously each node funded only itself, so state roots
    // diverged and peers rejected each other's blocks). Single-validator
    // quick-start mode funds just the local validator.
    let multi_validator = genesis_file
        .as_ref()
        .is_some_and(|g| g.validators.len() > 1);
    let (genesis_accounts, genesis_time_ms): (Vec<(Address, u128)>, u64) = match &genesis_file {
        Some(g) => (
            g.accounts.iter().map(|a| (a.address, a.balance)).collect(),
            g.genesis_time_ms()
                .or_else(|| node.stored_genesis_time_ms())
                .unwrap_or_else(unix_millis),
        ),
        None => (
            vec![(validator_address, chain_config.tokens.swr_initial_supply)],
            node.stored_genesis_time_ms().unwrap_or_else(unix_millis),
        ),
    };
    if node.apply_genesis(&genesis_accounts, genesis_time_ms)? {
        tracing::info!(
            accounts = genesis_accounts.len(),
            genesis_time_ms,
            "Genesis state applied"
        );
    } else {
        tracing::info!("Node already initialized, skipping genesis");
    }
    node.set_genesis_time_ms(genesis_time_ms);
    // Bind the persisted state to both genesis and execution configuration.
    use sha2::{Digest, Sha256};
    let genesis_hash = match &genesis_file {
        Some(genesis) => genesis.build().genesis_hash,
        None => H256::from(<[u8; 32]>::from(Sha256::digest(bincode::serialize(&(
            &genesis_accounts,
            validator_address,
            genesis_time_ms,
        ))?))),
    };
    let chain_identity = H256::from(<[u8; 32]>::from(Sha256::digest(serde_json::to_vec(&(
        chain_config.as_ref(),
        genesis_hash,
        genesis_file.as_ref(),
    ))?)));
    node.bind_chain_identity(chain_identity)?;

    // Set up P2P outbound channel with bounded capacity for backpressure.
    // If the P2P layer can't keep up, senders will drop messages rather than OOMing.
    const P2P_OUTBOUND_CAPACITY: usize = 4096;
    let (outbound_tx, outbound_rx) = mpsc::channel(P2P_OUTBOUND_CAPACITY);
    node.set_broadcast_tx(outbound_tx);

    // Set up P2P inbound channel with bounded capacity to prevent OOM from
    // peer message floods. Events are dropped (not blocked) when full.
    const P2P_INBOUND_CAPACITY: usize = 4096;
    let (net_tx, net_rx) = mpsc::channel(P2P_INBOUND_CAPACITY);

    // Shutdown coordination: a watch channel that signals all tasks to stop.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let shared_node = Arc::new(RwLock::new(node));

    let backend = NodeRpcBackend {
        genesis_hash,
        chain_identity,
        node: shared_node.clone(),
        airdrop_enabled: !multi_validator,
    };

    // Create RPC shutdown signal from the watch channel
    let rpc_shutdown_rx = shutdown_rx.clone();
    let rpc_per_sec: f64 = env::var("AETHER_RPC_RATE_LIMIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50.0);
    let rpc_bind_ip: std::net::IpAddr = env::var("AETHER_RPC_BIND_IP")
        .unwrap_or_else(|_| "127.0.0.1".to_owned())
        .parse()
        .context("AETHER_RPC_BIND_IP must be an IP address")?;
    let rpc_server =
        JsonRpcServer::with_chain_id(backend, rpc_port, chain_config.chain.chain_id_numeric)
            .with_bind_ip(rpc_bind_ip)
            .with_rate_limit((rpc_per_sec * 2.0) as u32, rpc_per_sec)
            .set_shutdown_signal(async move {
                let mut rx = rpc_shutdown_rx;
                // Wait until the value changes to true
                while !*rx.borrow() {
                    if rx.changed().await.is_err() {
                        return;
                    }
                }
            });

    // Initialize P2P network
    let mut p2p = P2PNetwork::new_random()?;
    let listen_addr = format!("/ip4/0.0.0.0/tcp/{}", p2p_port);
    p2p.start(&listen_addr).await?;
    let peer_id = p2p.peer_id_str();

    tracing::info!("Validator address: {:?}", validator_address);
    tracing::info!("Peer ID: {}", peer_id);
    tracing::info!("Consensus: VRF + HotStuff + BLS");
    tracing::info!("P2P listening on 0.0.0.0:{p2p_port}");
    tracing::info!("JSON-RPC listening on {rpc_bind_ip}:{rpc_port}");
    tracing::info!(
        "Slot duration: {}ms, Epoch: {} slots",
        chain_config.chain.slot_ms,
        chain_config.chain.epoch_slots
    );
    tracing::info!("Press Ctrl-C to stop.\n");

    // Connect to bootstrap peers if specified
    if let Ok(peers) = env::var("AETHER_BOOTSTRAP_PEERS") {
        for peer_addr in peers.split(',') {
            let addr = peer_addr.trim();
            if !addr.is_empty() {
                match p2p.connect_peer(addr).await {
                    Ok(()) => tracing::info!("Connecting to peer: {addr}"),
                    Err(e) => tracing::warn!("Failed to connect to {addr}: {e}"),
                }
            }
        }
    }

    let slot_task = tokio::spawn(run_slot_loop(
        shared_node.clone(),
        net_rx,
        shutdown_rx.clone(),
    ));
    // Poll the server on the runtime's main task. Warp's generic server future
    // does not need to migrate between workers; connection handling remains
    // asynchronous while the slot and P2P loops run on their own tasks.
    let rpc_task = rpc_server.run();
    tokio::pin!(rpc_task);
    let mut rpc_finished = false;
    let p2p_task = tokio::spawn(run_p2p_outbound(p2p, outbound_rx, net_tx, shutdown_rx));
    let metrics_addr: std::net::SocketAddr = ([0, 0, 0, 0], metrics_port).into();
    let metrics_task = tokio::spawn(async move {
        if let Err(e) = start_metrics_exporter(metrics_addr).await {
            tracing::error!(err = %e, "Metrics exporter failed");
        }
    });
    tracing::info!("Prometheus metrics on 0.0.0.0:{metrics_port}/metrics");

    // Wait for a shutdown signal (SIGINT or SIGTERM)
    tokio::select! {
        res = slot_task => {
            match res {
                Ok(inner) => inner?,
                Err(e) => return Err(anyhow::anyhow!("slot loop task failed: {e}")),
            }
        }
        res = &mut rpc_task => {
            rpc_finished = true;
            res?;
        }
        res = p2p_task => {
            match res {
                Ok(inner) => inner?,
                Err(e) => return Err(anyhow::anyhow!("p2p task failed: {e}")),
            }
        }
        _ = metrics_task => {}
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received SIGINT, initiating graceful shutdown...");
        }
        _ = sigterm_recv() => {
            tracing::info!("Received SIGTERM, initiating graceful shutdown...");
        }
    }

    // Signal all tasks to stop
    let _ = shutdown_tx.send(true);
    if !rpc_finished {
        match tokio::time::timeout(Duration::from_secs(2), &mut rpc_task).await {
            Ok(result) => result.context("stopping JSON-RPC server")?,
            Err(_) => tracing::warn!("JSON-RPC graceful shutdown deadline elapsed"),
        }
    }

    // Give tasks a short window to finish in-flight work
    tracing::info!("Waiting for tasks to finish...");
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Flush node state to disk
    match shared_node.write() {
        Ok(mut node) => {
            if let Err(e) = node.shutdown() {
                tracing::error!("Error during node shutdown: {e}");
            }
        }
        Err(_) => {
            tracing::error!("Node lock poisoned — cannot flush state");
        }
    }

    tracing::info!("Aether node exited cleanly");
    Ok(())
}
