use std::env;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use aether_metrics::exporter::start_metrics_exporter;
use aether_node::SyncRequest;
use aether_node::{
    create_hybrid_consensus, create_hybrid_consensus_with_all_keys, validator_info_from_keypair,
    GenesisConfig, Node, OutboundMessage, ValidatorKeypair,
};
use aether_p2p::network::{P2PNetwork, TOPIC_SYNC, TOPIC_VOTE};
use aether_rpc_json::{JsonRpcServer, RpcBackend};
use aether_types::{Address, Block, ChainConfig, Transaction, TransactionReceipt, H256};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};

struct NodeRpcBackend {
    node: Arc<RwLock<Node>>,
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
        node.try_get_block_by_slot(block_number)
    }

    fn get_block_by_hash(&self, block_hash: H256, _full_tx: bool) -> Result<Option<Block>> {
        let node = self.read_node()?;
        node.try_get_block_by_hash(block_hash)
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
        self.read_node()
            .map(|node| node.allows_airdrop())
            .unwrap_or(false)
    }

    fn request_airdrop(&self, address: Address, amount: u128) -> Result<()> {
        let mut node = self.write_node()?;
        node.seed_account(&address, amount)
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
    slot_ms: u64,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let mut inbound_drops: u64 = 0;

    loop {
        // Check for shutdown signal before each tick
        if *shutdown_rx.borrow() {
            tracing::info!("Slot loop received shutdown signal, stopping");
            return Ok(());
        }

        // Drain pending network messages, bounded per tick to prevent lock
        // starvation. If the channel is flooded, remaining events are
        // deferred to the next tick so slot production stays on schedule.
        {
            let mut guard = node
                .write()
                .map_err(|_| anyhow::anyhow!("node lock poisoned"))?;
            let mut processed = 0usize;
            while processed < MAX_EVENTS_PER_TICK {
                match net_rx.try_recv() {
                    Ok(event) => {
                        guard.handle_network_event(event)?;
                        processed += 1;
                    }
                    Err(_) => break,
                }
            }
            if processed == MAX_EVENTS_PER_TICK {
                // We hit the per-tick cap. Remaining events stay in the
                // bounded channel and will be processed on the next tick.
                // If the channel is persistently full, the P2P layer's
                // try_send will start dropping new events at the source.
                inbound_drops += 1;
                tracing::warn!(
                    processed,
                    backpressure_ticks = inbound_drops,
                    "Inbound event drain capped — deferring remaining events to next tick"
                );
            }
        }

        // Tick the node
        {
            let mut guard = node
                .write()
                .map_err(|_| anyhow::anyhow!("node lock poisoned"))?;
            guard.tick()?;
        }

        // Sleep with shutdown awareness — wake early if shutdown is signaled
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(slot_ms)) => {}
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
                        if let Err(e) = p2p.broadcast_transaction(&tx) {
                            tracing::warn!("failed to broadcast tx: {e}");
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

    // Load chain configuration
    let chain_config = if let Ok(config_path) = env::var("AETHER_CONFIG_PATH") {
        tracing::info!("Loading config from: {config_path}");
        ChainConfig::from_toml_file(Path::new(&config_path))?
    } else {
        let network = env::var("AETHER_NETWORK").unwrap_or_else(|_| "devnet".to_string());
        tracing::info!("Using {network} preset config");
        match network.as_str() {
            "mainnet" => ChainConfig::mainnet(),
            "testnet" => ChainConfig::testnet(),
            _ => ChainConfig::devnet(),
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
        // Auto-save so the node keeps the same identity on restart
        if let Err(e) = kp.save_to_file(key_path) {
            tracing::warn!(err = %e, "failed to save validator key");
        } else {
            tracing::info!(path = %key_path.display(), "Saved validator key");
        }
        kp
    };
    let validator_address = validator_keypair.address();

    // Build consensus from genesis file (multi-validator) or single-validator mode
    let consensus: Box<dyn aether_consensus::ConsensusEngine> =
        if let Ok(genesis_path) = env::var("AETHER_GENESIS_PATH") {
            tracing::info!(path = %genesis_path, "Loading genesis config");
            let genesis_bytes = std::fs::read(&genesis_path)
                .with_context(|| format!("failed to read genesis file: {genesis_path}"))?;
            let genesis: GenesisConfig = serde_json::from_slice(&genesis_bytes)
                .with_context(|| "failed to parse genesis JSON")?;
            genesis.validate()?;

            let result = genesis.build();
            let vrf_pubkeys = genesis.vrf_pubkeys();
            let bls_pubkeys = genesis.bls_pubkeys();

            tracing::info!(
                validators = result.validator_set.len(),
                total_stake = result.total_stake,
                "Genesis config loaded"
            );

            Box::new(create_hybrid_consensus_with_all_keys(
                result.validator_set,
                vrf_pubkeys,
                bls_pubkeys,
                Some(&validator_keypair),
                chain_config.consensus.tau,
                chain_config.chain.epoch_slots,
            )?)
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

    // Seed validator with genesis balance (only on first run)
    let genesis_balance = chain_config.tokens.swr_initial_supply;
    if node.get_account(validator_address)?.is_none() {
        node.seed_account(&validator_address, genesis_balance)?;
        tracing::info!("Genesis: funded validator with {genesis_balance}");
    } else {
        tracing::info!("Node already initialized, skipping genesis funding");
    }

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
        node: shared_node.clone(),
    };

    // Create RPC shutdown signal from the watch channel
    let rpc_shutdown_rx = shutdown_rx.clone();
    let rpc_server = JsonRpcServer::new(backend, rpc_port).set_shutdown_signal(async move {
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
    tracing::info!("JSON-RPC listening on 127.0.0.1:{rpc_port}");
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
                match p2p.connect_peer(addr) {
                    Ok(()) => tracing::info!("Connecting to peer: {addr}"),
                    Err(e) => tracing::warn!("Failed to connect to {addr}: {e}"),
                }
            }
        }
    }

    let slot_ms = chain_config.chain.slot_ms;
    let slot_task = tokio::spawn(run_slot_loop(
        shared_node.clone(),
        net_rx,
        slot_ms,
        shutdown_rx.clone(),
    ));
    let rpc_task = tokio::spawn(async move { rpc_server.run().await });
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
        res = rpc_task => {
            match res {
                Ok(inner) => inner?,
                Err(e) => return Err(anyhow::anyhow!("rpc task failed: {e}")),
            }
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

    // Give tasks a bounded window to finish in-flight work
    tracing::info!("Waiting for tasks to finish (5s deadline)...");
    tokio::time::sleep(Duration::from_secs(5)).await;

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
