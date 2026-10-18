use aether_metrics::p2p::topic_label;
use aether_metrics::{NET_METRICS, P2P_METRICS};
use aether_types::{Block, Transaction};
use anyhow::Result;
use libp2p::connection_limits::{self, ConnectionLimits};
use libp2p::futures::StreamExt;
use libp2p::{
    gossipsub::{self, IdentTopic, MessageAuthenticity, ValidationMode},
    identify,
    identity::Keypair,
    kad, noise,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, Multiaddr, PeerId, Swarm, SwarmBuilder,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Topics for Aether network gossip.
pub const TOPIC_TX: &str = "/aether/1/tx";
pub const TOPIC_BLOCK: &str = "/aether/1/block";
pub const TOPIC_VOTE: &str = "/aether/1/vote";
pub const TOPIC_SHRED: &str = "/aether/1/shred";
pub const TOPIC_SYNC: &str = "/aether/1/sync";

/// Per-topic maximum message sizes (bytes).
/// Transactions are small (~1-2 KB typical, 64 KB generous max).
/// Votes are BLS signatures + metadata (~512 bytes typical, 8 KB max).
/// Shreds are erasure-coded block fragments. With RS(10,2) on a 2 MB block,
/// each shard payload is ceil((2 MB + 8) / 10) ≈ 210 KB plus ~200 B of
/// shred metadata, so 256 KB is the minimum safe limit.
/// Blocks can be large but still bounded (2 MB via gossipsub max_transmit_size).
const MAX_TX_SIZE: usize = 1024 * 1024; // 1 MB — transaction gossip is batched
const MAX_BLOCK_SIZE: usize = 2 * 1024 * 1024; // 2 MB
const MAX_VOTE_SIZE: usize = 8 * 1024; // 8 KB
const MAX_SHRED_SIZE: usize = 256 * 1024; // 256 KB — RS(10,2) on 2 MB block ≈ 210 KB per shred
const MAX_SYNC_MSG_SIZE: usize = 1024; // 1 KB (slot range requests are small)

/// Maximum total established connections (inbound + outbound).
const MAX_ESTABLISHED_TOTAL: u32 = 256;
/// Maximum established inbound connections. Limits DoS via connection flooding.
const MAX_ESTABLISHED_INBOUND: u32 = 128;
/// Maximum established outbound connections.
const MAX_ESTABLISHED_OUTBOUND: u32 = 128;
/// Maximum established connections per single peer (prevents resource hogging).
const MAX_ESTABLISHED_PER_PEER: u32 = 4;

/// Events emitted by the P2P network to the node.
#[derive(Debug)]
pub enum NetworkEvent {
    TransactionReceived(Vec<u8>),
    BlockReceived(Vec<u8>),
    VoteReceived(Vec<u8>),
    ShredReceived(Vec<u8>),
    SyncRequestReceived(Vec<u8>),
    PeerConnected(PeerId),
    PeerDisconnected(PeerId),
}

/// Composite libp2p behaviour for Aether.
#[derive(NetworkBehaviour)]
struct AetherBehaviour {
    gossipsub: gossipsub::Behaviour,
    kademlia: kad::Behaviour<kad::store::MemoryStore>,
    identify: identify::Behaviour,
    connection_limits: connection_limits::Behaviour,
}

/// Production P2P network using libp2p.
/// Ban duration for misbehaving peers (1 hour).
const BAN_DURATION_SECS: u64 = 3600;

const MAX_BANNED_PEERS: usize = 4096;

// Validators exchange blocks, votes, sync responses and batched tx gossip;
// 100 msgs/s (the old limit) banned honest peers within seconds under load.
const RATE_LIMIT_TOKENS: u32 = 2000;
const RATE_LIMIT_REFILL_INTERVAL: Duration = Duration::from_secs(1);
const RATE_LIMIT_PENALTY: i32 = -20;
const MAX_RATE_LIMITERS: usize = 1024;

struct PeerRateLimiter {
    tokens: u32,
    last_refill: Instant,
}

impl PeerRateLimiter {
    fn new() -> Self {
        Self {
            tokens: RATE_LIMIT_TOKENS,
            last_refill: Instant::now(),
        }
    }

    fn try_consume(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill);
        if elapsed >= RATE_LIMIT_REFILL_INTERVAL {
            let refills = (elapsed.as_millis() / RATE_LIMIT_REFILL_INTERVAL.as_millis()) as u32;
            self.tokens =
                RATE_LIMIT_TOKENS.min(self.tokens.saturating_add(refills * RATE_LIMIT_TOKENS));
            self.last_refill = now;
        }
        if self.tokens > 0 {
            self.tokens -= 1;
            true
        } else {
            false
        }
    }
}

pub struct P2PNetwork {
    swarm: Swarm<AetherBehaviour>,
    local_peer_id: PeerId,
    topics: HashMap<String, IdentTopic>,
    event_tx: mpsc::Sender<NetworkEvent>,
    event_rx: mpsc::Receiver<NetworkEvent>,
    peers: HashMap<PeerId, PeerInfo>,
    /// Banned peers with expiry timestamps. Peers cannot reconnect until ban expires.
    banned_peers: HashMap<PeerId, u64>,
    rate_limiters: HashMap<PeerId, PeerRateLimiter>,
}

#[derive(Clone, Debug)]
pub struct PeerInfo {
    pub id: String,
    pub address: String,
    pub score: i32,
    pub connected_at: u64,
}

impl P2PNetwork {
    /// Create a new P2P network with a random keypair.
    pub fn new(keypair: Keypair) -> Result<Self> {
        let local_peer_id = PeerId::from(keypair.public());

        // Configure gossipsub
        let gossipsub_config = gossipsub::ConfigBuilder::default()
            .heartbeat_interval(Duration::from_secs(1))
            .validation_mode(ValidationMode::Strict)
            .mesh_n(8)
            .mesh_n_low(4)
            .mesh_n_high(12)
            .gossip_lazy(6)
            .history_length(5)
            .history_gossip(3)
            .max_transmit_size(2 * 1024 * 1024) // 2MB blocks
            .build()
            .map_err(|e| anyhow::anyhow!("gossipsub config error: {}", e))?;

        let gossipsub = gossipsub::Behaviour::new(
            MessageAuthenticity::Signed(keypair.clone()),
            gossipsub_config,
        )
        .map_err(|e| anyhow::anyhow!("gossipsub init error: {}", e))?;

        // Configure Kademlia
        let store = kad::store::MemoryStore::new(local_peer_id);
        let kademlia = kad::Behaviour::new(local_peer_id, store);

        // Configure Identify
        let identify = identify::Behaviour::new(identify::Config::new(
            "/aether/1.0.0".to_string(),
            keypair.public(),
        ));

        let limits = ConnectionLimits::default()
            .with_max_established(Some(MAX_ESTABLISHED_TOTAL))
            .with_max_established_incoming(Some(MAX_ESTABLISHED_INBOUND))
            .with_max_established_outgoing(Some(MAX_ESTABLISHED_OUTBOUND))
            .with_max_established_per_peer(Some(MAX_ESTABLISHED_PER_PEER));

        let behaviour = AetherBehaviour {
            gossipsub,
            kademlia,
            identify,
            connection_limits: connection_limits::Behaviour::new(limits),
        };

        let swarm = SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                noise::Config::new,
                yamux::Config::default,
            )?
            .with_behaviour(|_| Ok(behaviour))
            .map_err(|e| anyhow::anyhow!("swarm build error: {}", e))?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();

        let (event_tx, event_rx) = mpsc::channel(1024);

        Ok(P2PNetwork {
            swarm,
            local_peer_id,
            topics: HashMap::new(),
            event_tx,
            event_rx,
            peers: HashMap::new(),
            banned_peers: HashMap::new(),
            rate_limiters: HashMap::new(),
        })
    }

    /// Create with a random keypair (convenience).
    pub fn new_random() -> Result<Self> {
        let keypair = Keypair::generate_ed25519();
        Self::new(keypair)
    }

    pub fn peer_id(&self) -> &PeerId {
        &self.local_peer_id
    }

    pub fn peer_id_str(&self) -> String {
        self.local_peer_id.to_string()
    }

    /// Start listening on the given address.
    pub async fn start(&mut self, listen_addr: &str) -> Result<()> {
        let addr: Multiaddr = listen_addr
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid listen address: {}", e))?;
        self.swarm.listen_on(addr)?;

        // Subscribe to all standard topics
        self.subscribe(TOPIC_TX)?;
        self.subscribe(TOPIC_BLOCK)?;
        self.subscribe(TOPIC_VOTE)?;
        self.subscribe(TOPIC_SHRED)?;
        self.subscribe(TOPIC_SYNC)?;

        Ok(())
    }

    /// Subscribe to a gossipsub topic.
    pub fn subscribe(&mut self, topic_str: &str) -> Result<()> {
        if self.topics.contains_key(topic_str) {
            return Ok(());
        }
        let topic = IdentTopic::new(topic_str);
        self.swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&topic)
            .map_err(|e| anyhow::anyhow!("subscribe error: {}", e))?;
        self.topics.insert(topic_str.to_string(), topic);
        Ok(())
    }

    /// Publish data to a gossipsub topic.
    ///
    /// Validates outbound message size against per-topic limits before sending.
    /// This prevents our node from broadcasting messages that peers will reject
    /// and penalize us for.
    pub fn publish(&mut self, topic_str: &str, data: Vec<u8>) -> Result<()> {
        let topic = self
            .topics
            .get(topic_str)
            .ok_or_else(|| anyhow::anyhow!("not subscribed to topic: {}", topic_str))?;
        let size = data.len();

        let max_size = max_size_for_topic(topic_str);
        if size == 0 {
            return Err(anyhow::anyhow!(
                "refusing to publish empty message to {}",
                topic_str
            ));
        }
        if size > max_size {
            return Err(anyhow::anyhow!(
                "refusing to publish oversized message to {}: {} bytes > {} max",
                topic_str,
                size,
                max_size
            ));
        }

        self.swarm
            .behaviour_mut()
            .gossipsub
            .publish(topic.clone(), data)
            .map_err(|e| anyhow::anyhow!("publish error: {}", e))?;
        NET_METRICS.messages_sent.inc();
        NET_METRICS.message_size_bytes.observe(size as f64);
        Ok(())
    }

    /// Broadcast a transaction.
    pub fn broadcast_transaction(&mut self, tx: &Transaction) -> Result<()> {
        let _span = tracing::debug_span!("broadcast_tx", fee = tx.fee).entered();
        let data = bincode::serialize(tx)?;
        self.publish(TOPIC_TX, data)
    }

    /// Broadcast a block.
    pub fn broadcast_block(&mut self, block: &Block) -> Result<()> {
        let _span = tracing::debug_span!(
            "broadcast_block",
            slot = block.header.slot,
            tx_count = block.transactions.len(),
        )
        .entered();
        let data = bincode::serialize(block)?;
        self.publish(TOPIC_BLOCK, data)
    }

    /// Connect to a peer by multiaddr. Refuses to dial banned peers.
    pub fn connect_peer(&mut self, addr: &str) -> Result<()> {
        let multiaddr: Multiaddr = addr
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid multiaddr: {}", e))?;

        // Extract peer ID from multiaddr and reject if banned
        if let Some(libp2p::multiaddr::Protocol::P2p(peer_id)) = multiaddr.iter().last() {
            if self.is_banned(&peer_id) {
                return Err(anyhow::anyhow!(
                    "peer {} is banned, refusing to dial",
                    peer_id
                ));
            }
        }

        self.swarm.dial(multiaddr)?;
        Ok(())
    }

    /// Add a known bootstrap peer for Kademlia.
    pub fn add_bootstrap_peer(&mut self, peer_id: PeerId, addr: Multiaddr) {
        self.swarm
            .behaviour_mut()
            .kademlia
            .add_address(&peer_id, addr);
    }

    /// Get connected peer count.
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Get connected peers.
    pub fn get_peers(&self) -> Vec<&PeerInfo> {
        self.peers.values().collect()
    }

    /// Take the event receiver (for the node to consume events).
    pub fn take_event_rx(&mut self) -> mpsc::Receiver<NetworkEvent> {
        let (new_tx, new_rx) = mpsc::channel(1024);
        let old_rx = std::mem::replace(&mut self.event_rx, new_rx);
        self.event_tx = new_tx;
        old_rx
    }

    /// Poll the swarm for events. Call this in a loop from the node.
    pub async fn poll(&mut self) -> Option<NetworkEvent> {
        loop {
            match self.swarm.select_next_some().await {
                SwarmEvent::Behaviour(AetherBehaviourEvent::Gossipsub(
                    gossipsub::Event::Message {
                        message,
                        propagation_source,
                        ..
                    },
                )) => {
                    // Drop messages from banned peers that arrived before disconnect
                    if self.is_banned(&propagation_source) {
                        P2P_METRICS.messages_dropped_banned.inc();
                        let _ = self.swarm.disconnect_peer_id(propagation_source);
                        continue;
                    }

                    if !self.check_rate_limit(&propagation_source) {
                        P2P_METRICS.messages_dropped_rate_limited.inc();
                        self.update_peer_score(&propagation_source, RATE_LIMIT_PENALTY);
                        continue;
                    }

                    let topic = message.topic.to_string();
                    let data = message.data;
                    let size = data.len();

                    // Per-topic message size validation.
                    // Uses exact topic matching (not substring) to prevent
                    // misclassification of similarly-named topics.
                    let (max_size, event_fn): (usize, fn(Vec<u8>) -> NetworkEvent) =
                        if topic == TOPIC_TX {
                            (MAX_TX_SIZE, NetworkEvent::TransactionReceived)
                        } else if topic == TOPIC_BLOCK {
                            (MAX_BLOCK_SIZE, NetworkEvent::BlockReceived)
                        } else if topic == TOPIC_VOTE {
                            (MAX_VOTE_SIZE, NetworkEvent::VoteReceived)
                        } else if topic == TOPIC_SHRED {
                            (MAX_SHRED_SIZE, NetworkEvent::ShredReceived)
                        } else if topic == TOPIC_SYNC {
                            (MAX_SYNC_MSG_SIZE, NetworkEvent::SyncRequestReceived)
                        } else {
                            continue;
                        };

                    let label = topic_label(&topic);

                    if size == 0 || size > max_size {
                        tracing::warn!(
                            peer = %propagation_source,
                            topic = %topic,
                            size,
                            max_size,
                            "dropping oversized gossipsub message, penalizing peer"
                        );
                        P2P_METRICS
                            .messages_dropped_oversized
                            .with_label_values(&[label])
                            .inc();
                        self.update_peer_score(&propagation_source, -10);
                        continue;
                    }

                    let event = event_fn(data);
                    NET_METRICS.messages_received.inc();
                    NET_METRICS.message_size_bytes.observe(size as f64);
                    P2P_METRICS
                        .messages_received_by_topic
                        .with_label_values(&[label])
                        .inc();

                    return Some(event);
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    // Check if peer is banned
                    if let Some(&ban_expiry) = self.banned_peers.get(&peer_id) {
                        if current_timestamp() < ban_expiry {
                            // Still banned — disconnect immediately
                            let _ = self.swarm.disconnect_peer_id(peer_id);
                            continue;
                        } else {
                            // Ban expired — remove from list
                            self.banned_peers.remove(&peer_id);
                        }
                    }

                    let info = PeerInfo {
                        id: peer_id.to_string(),
                        address: String::new(),
                        score: 0,
                        connected_at: current_timestamp(),
                    };
                    self.peers.insert(peer_id, info);
                    NET_METRICS.connections_total.inc();
                    NET_METRICS.peers_connected.set(self.peers.len() as i64);
                    return Some(NetworkEvent::PeerConnected(peer_id));
                }
                SwarmEvent::ConnectionClosed { peer_id, .. } => {
                    self.peers.remove(&peer_id);
                    self.rate_limiters.remove(&peer_id);
                    NET_METRICS.peers_connected.set(self.peers.len() as i64);
                    return Some(NetworkEvent::PeerDisconnected(peer_id));
                }
                SwarmEvent::NewListenAddr { address, .. } => {
                    tracing::info!("Listening on {}/p2p/{}", address, self.local_peer_id);
                    continue;
                }
                _ => continue,
            }
        }
    }

    /// Update a peer's reputation score.
    pub fn update_peer_score(&mut self, peer_id: &PeerId, delta: i32) {
        if let Some(info) = self.peers.get_mut(peer_id) {
            info.score += delta;
            if info.score < -100 {
                // Ban for BAN_DURATION_SECS, disconnect
                P2P_METRICS.peers_banned.inc();
                let ban_expiry = current_timestamp() + BAN_DURATION_SECS;
                self.banned_peers.insert(*peer_id, ban_expiry);
                let _ = self.swarm.disconnect_peer_id(*peer_id);
                self.peers.remove(peer_id);
                self.rate_limiters.remove(peer_id);
                // Prevent unbounded growth of the ban list.
                if self.banned_peers.len() > MAX_BANNED_PEERS {
                    self.prune_banned_peers();
                }
            }
        }
    }

    fn check_rate_limit(&mut self, peer_id: &PeerId) -> bool {
        if self.rate_limiters.len() >= MAX_RATE_LIMITERS
            && !self.rate_limiters.contains_key(peer_id)
        {
            self.rate_limiters
                .retain(|pid, _| self.peers.contains_key(pid));
        }
        self.rate_limiters
            .entry(*peer_id)
            .or_insert_with(PeerRateLimiter::new)
            .try_consume()
    }

    /// Check if a peer is currently banned.
    pub fn is_banned(&self, peer_id: &PeerId) -> bool {
        self.banned_peers
            .get(peer_id)
            .is_some_and(|&expiry| current_timestamp() < expiry)
    }

    /// Get count of currently banned peers.
    pub fn banned_count(&self) -> usize {
        let now = current_timestamp();
        self.banned_peers
            .values()
            .filter(|&&expiry| now < expiry)
            .count()
    }

    /// Remove expired bans and, if still over capacity, evict oldest bans.
    fn prune_banned_peers(&mut self) {
        let now = current_timestamp();
        // Phase 1: remove all expired bans.
        self.banned_peers.retain(|_, &mut expiry| expiry > now);
        // Phase 2: if still over cap, evict soonest-to-expire entries.
        if self.banned_peers.len() > MAX_BANNED_PEERS {
            let mut entries: Vec<(PeerId, u64)> =
                self.banned_peers.iter().map(|(&k, &v)| (k, v)).collect();
            // Sort by expiry ascending — evict soonest-to-expire first.
            entries.sort_by_key(|&(_, expiry)| expiry);
            let to_remove = self.banned_peers.len() - MAX_BANNED_PEERS;
            for (peer_id, _) in entries.into_iter().take(to_remove) {
                self.banned_peers.remove(&peer_id);
            }
        }
    }
}

/// Map a topic string to its per-topic maximum message size.
/// Returns the gossipsub global max (2 MB) for unknown topics as a safe fallback.
fn max_size_for_topic(topic: &str) -> usize {
    match topic {
        TOPIC_TX => MAX_TX_SIZE,
        TOPIC_BLOCK => MAX_BLOCK_SIZE,
        TOPIC_VOTE => MAX_VOTE_SIZE,
        TOPIC_SHRED => MAX_SHRED_SIZE,
        TOPIC_SYNC => MAX_SYNC_MSG_SIZE,
        _ => MAX_BLOCK_SIZE,
    }
}

fn current_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_network_creation() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let network = P2PNetwork::new_random().unwrap();
            assert_eq!(network.peer_count(), 0);
            assert!(!network.peer_id_str().is_empty());
        });
    }

    #[test]
    fn test_subscribe_topics() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut network = P2PNetwork::new_random().unwrap();
            network.subscribe(TOPIC_TX).unwrap();
            network.subscribe(TOPIC_BLOCK).unwrap();
            assert_eq!(network.topics.len(), 2);
        });
    }

    #[tokio::test]
    async fn test_two_nodes_connect() {
        let mut node1 = P2PNetwork::new_random().unwrap();
        let mut node2 = P2PNetwork::new_random().unwrap();

        node1.start("/ip4/127.0.0.1/tcp/0").await.unwrap();
        node2.start("/ip4/127.0.0.1/tcp/0").await.unwrap();

        // Poll node1 to process the NewListenAddr event
        let mut node1_addr = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), node1.swarm.select_next_some())
                .await
            {
                Ok(SwarmEvent::NewListenAddr { address, .. }) => {
                    node1_addr = Some(address);
                    break;
                }
                _ => continue,
            }
        }

        let node1_addr = node1_addr.expect("node1 should have a listen address");
        let dial_addr = format!("{}/p2p/{}", node1_addr, node1.local_peer_id);
        node2.connect_peer(&dial_addr).unwrap();

        // Poll both nodes to process connection events
        let mut connected = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline && !connected {
            tokio::select! {
                event = node1.swarm.select_next_some() => {
                    if matches!(event, SwarmEvent::ConnectionEstablished { .. }) {
                        connected = true;
                    }
                }
                event = node2.swarm.select_next_some() => {
                    if matches!(event, SwarmEvent::ConnectionEstablished { .. }) {
                        connected = true;
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }

        assert!(connected, "nodes should connect to each other");
    }

    #[test]
    fn test_ban_peer_and_check() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut network = P2PNetwork::new_random().unwrap();
            let peer_id = PeerId::random();

            // Peer starts unbanned
            assert!(!network.is_banned(&peer_id));
            assert_eq!(network.banned_count(), 0);

            // Ban the peer
            let ban_expiry = current_timestamp() + BAN_DURATION_SECS;
            network.banned_peers.insert(peer_id, ban_expiry);

            assert!(network.is_banned(&peer_id));
            assert_eq!(network.banned_count(), 1);
        });
    }

    #[test]
    fn test_ban_via_low_score() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut network = P2PNetwork::new_random().unwrap();
            let peer_id = PeerId::random();

            // Add the peer to connected peers
            network.peers.insert(
                peer_id,
                PeerInfo {
                    id: peer_id.to_string(),
                    address: String::new(),
                    score: 0,
                    connected_at: current_timestamp(),
                },
            );

            // Decrease score below -100 threshold
            network.update_peer_score(&peer_id, -101);

            // Peer should be banned and removed from connected peers
            assert!(network.is_banned(&peer_id));
            assert_eq!(network.peer_count(), 0);
        });
    }

    #[test]
    fn test_connect_peer_rejects_banned() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut network = P2PNetwork::new_random().unwrap();
            let banned_peer_id = PeerId::random();

            // Ban the peer
            let ban_expiry = current_timestamp() + BAN_DURATION_SECS;
            network.banned_peers.insert(banned_peer_id, ban_expiry);

            // Attempt to dial banned peer — should fail
            let addr = format!("/ip4/127.0.0.1/tcp/9999/p2p/{}", banned_peer_id);
            let result = network.connect_peer(&addr);
            assert!(result.is_err());
            assert!(
                result.unwrap_err().to_string().contains("banned"),
                "error should mention peer is banned"
            );
        });
    }

    #[test]
    fn test_message_size_constants() {
        // Use runtime values to avoid clippy assertions_on_constants
        let tx = MAX_TX_SIZE;
        let block = MAX_BLOCK_SIZE;
        let vote = MAX_VOTE_SIZE;
        let shred = MAX_SHRED_SIZE;
        let global_max = 2 * 1024 * 1024usize;

        assert!(tx > 0 && block > 0 && vote > 0 && shred > 0);
        assert!(tx <= global_max);
        assert!(block <= global_max);
        assert!(vote <= global_max);
        assert!(shred <= global_max);
        assert!(vote < tx);
        assert!(tx < block);
    }

    #[test]
    fn test_connection_limits_configured() {
        // Verify connection limits are sane production values (use runtime values)
        let total = MAX_ESTABLISHED_TOTAL;
        let inbound = MAX_ESTABLISHED_INBOUND;
        let outbound = MAX_ESTABLISHED_OUTBOUND;
        let per_peer = MAX_ESTABLISHED_PER_PEER;
        assert!(total >= inbound);
        assert!(total >= outbound);
        assert!(per_peer > 0 && per_peer <= 8);
        // Verify the network can be created with limits wired into the swarm
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let network = P2PNetwork::new_random().unwrap();
            assert_eq!(network.peer_count(), 0);
        });
    }

    #[test]
    fn test_ban_expiry() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut network = P2PNetwork::new_random().unwrap();
            let peer_id = PeerId::random();

            // Ban with an already-expired timestamp
            network
                .banned_peers
                .insert(peer_id, current_timestamp() - 1);

            // Should not be considered banned
            assert!(!network.is_banned(&peer_id));
            assert_eq!(network.banned_count(), 0);

            // Dialing should succeed (won't actually connect, but won't be rejected)
            let addr = format!("/ip4/127.0.0.1/tcp/9999/p2p/{}", peer_id);
            // This will fail to connect but NOT because of ban
            let result = network.connect_peer(&addr);
            // The dial itself should be accepted (connection will fail async)
            assert!(result.is_ok());
        });
    }

    #[test]
    fn test_prune_banned_peers_removes_expired() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut network = P2PNetwork::new_random().unwrap();
            let now = current_timestamp();

            // Insert some expired and some active bans
            for _ in 0..10 {
                network.banned_peers.insert(PeerId::random(), now - 1); // expired
            }
            let active_peer = PeerId::random();
            network.banned_peers.insert(active_peer, now + 3600); // active

            assert_eq!(network.banned_peers.len(), 11);
            network.prune_banned_peers();
            // Only the active ban should remain
            assert_eq!(network.banned_peers.len(), 1);
            assert!(network.banned_peers.contains_key(&active_peer));
        });
    }

    #[test]
    fn test_prune_banned_peers_evicts_oldest_when_over_cap() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut network = P2PNetwork::new_random().unwrap();
            let now = current_timestamp();

            // Fill beyond MAX_BANNED_PEERS with active bans
            for i in 0..(MAX_BANNED_PEERS + 100) {
                network
                    .banned_peers
                    .insert(PeerId::random(), now + 3600 + i as u64);
            }
            assert_eq!(network.banned_peers.len(), MAX_BANNED_PEERS + 100);
            network.prune_banned_peers();
            assert_eq!(network.banned_peers.len(), MAX_BANNED_PEERS);
        });
    }

    #[test]
    fn test_rate_limiter_allows_up_to_limit() {
        let mut limiter = PeerRateLimiter::new();
        for _ in 0..RATE_LIMIT_TOKENS {
            assert!(limiter.try_consume());
        }
        assert!(!limiter.try_consume());
    }

    #[test]
    fn test_rate_limiter_refills_after_interval() {
        let mut limiter = PeerRateLimiter::new();
        for _ in 0..RATE_LIMIT_TOKENS {
            limiter.try_consume();
        }
        assert!(!limiter.try_consume());
        limiter.last_refill = Instant::now() - RATE_LIMIT_REFILL_INTERVAL;
        assert!(limiter.try_consume());
    }

    #[test]
    fn test_peer_rate_limit_check() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut network = P2PNetwork::new_random().unwrap();
            let peer_id = PeerId::random();

            for _ in 0..RATE_LIMIT_TOKENS {
                assert!(network.check_rate_limit(&peer_id));
            }
            assert!(!network.check_rate_limit(&peer_id));
            assert_eq!(network.rate_limiters.len(), 1);
        });
    }

    #[test]
    fn test_rate_limiter_cleaned_on_disconnect() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut network = P2PNetwork::new_random().unwrap();
            let peer_id = PeerId::random();

            network.check_rate_limit(&peer_id);
            assert!(network.rate_limiters.contains_key(&peer_id));

            network.rate_limiters.remove(&peer_id);
            assert!(!network.rate_limiters.contains_key(&peer_id));
        });
    }

    #[test]
    fn test_rate_limit_triggers_score_penalty() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut network = P2PNetwork::new_random().unwrap();
            let peer_id = PeerId::random();

            network.peers.insert(
                peer_id,
                PeerInfo {
                    id: peer_id.to_string(),
                    address: String::new(),
                    score: 0,
                    connected_at: current_timestamp(),
                },
            );

            for _ in 0..RATE_LIMIT_TOKENS {
                network.check_rate_limit(&peer_id);
            }
            assert!(!network.check_rate_limit(&peer_id));
            network.update_peer_score(&peer_id, RATE_LIMIT_PENALTY);

            let score = network.peers.get(&peer_id).map(|p| p.score).unwrap_or(0);
            assert_eq!(score, RATE_LIMIT_PENALTY);
        });
    }

    #[test]
    fn test_rate_limiter_map_bounded() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut network = P2PNetwork::new_random().unwrap();

            for _ in 0..MAX_RATE_LIMITERS {
                let peer_id = PeerId::random();
                network.peers.insert(
                    peer_id,
                    PeerInfo {
                        id: peer_id.to_string(),
                        address: String::new(),
                        score: 0,
                        connected_at: current_timestamp(),
                    },
                );
                network.check_rate_limit(&peer_id);
            }
            assert_eq!(network.rate_limiters.len(), MAX_RATE_LIMITERS);

            let extra_peer = PeerId::random();
            network.check_rate_limit(&extra_peer);
            // After pruning disconnected peers + adding new one, should be bounded
            assert!(network.rate_limiters.len() <= MAX_RATE_LIMITERS + 1);
        });
    }

    #[test]
    fn test_ban_list_bounded_via_update_peer_score() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut network = P2PNetwork::new_random().unwrap();

            // Pre-fill banned_peers to just at the cap with expired entries
            let now = current_timestamp();
            for _ in 0..MAX_BANNED_PEERS {
                network.banned_peers.insert(PeerId::random(), now - 1);
            }
            assert_eq!(network.banned_peers.len(), MAX_BANNED_PEERS);

            // Add a connected peer and trigger a ban via low score
            let peer_id = PeerId::random();
            network.peers.insert(
                peer_id,
                PeerInfo {
                    id: peer_id.to_string(),
                    address: String::new(),
                    score: 0,
                    connected_at: now,
                },
            );
            network.update_peer_score(&peer_id, -101);

            // The new ban is added (len = MAX+1), triggering prune which
            // removes all expired entries, leaving only the new active ban.
            assert!(network.is_banned(&peer_id));
            assert_eq!(network.banned_peers.len(), 1);
        });
    }

    #[test]
    fn test_max_size_for_topic() {
        assert_eq!(max_size_for_topic(TOPIC_TX), MAX_TX_SIZE);
        assert_eq!(max_size_for_topic(TOPIC_BLOCK), MAX_BLOCK_SIZE);
        assert_eq!(max_size_for_topic(TOPIC_VOTE), MAX_VOTE_SIZE);
        assert_eq!(max_size_for_topic(TOPIC_SHRED), MAX_SHRED_SIZE);
        assert_eq!(max_size_for_topic(TOPIC_SYNC), MAX_SYNC_MSG_SIZE);
        // Unknown topics fall back to the global max (2 MB)
        assert_eq!(max_size_for_topic("/aether/1/unknown"), MAX_BLOCK_SIZE);
    }

    #[test]
    fn test_publish_rejects_empty_message() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut network = P2PNetwork::new_random().unwrap();
            network.subscribe(TOPIC_TX).unwrap();

            let result = network.publish(TOPIC_TX, vec![]);
            assert!(result.is_err());
            assert!(
                result.unwrap_err().to_string().contains("empty"),
                "error should mention empty message"
            );
        });
    }

    #[test]
    fn test_publish_rejects_oversized_message() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut network = P2PNetwork::new_random().unwrap();
            network.subscribe(TOPIC_TX).unwrap();
            network.subscribe(TOPIC_VOTE).unwrap();

            // TX at exactly the limit should succeed (publish itself may fail
            // because we have no peers, but the size check should pass)
            let at_limit = vec![0u8; MAX_TX_SIZE];
            let result = network.publish(TOPIC_TX, at_limit);
            // May fail with "InsufficientPeers" which is fine — the size check passed
            let is_size_error = result
                .as_ref()
                .err()
                .is_some_and(|e| e.to_string().contains("oversized"));
            assert!(
                !is_size_error,
                "message at limit should not be rejected as oversized"
            );

            // TX over the limit must be rejected before hitting gossipsub
            let oversized = vec![0u8; MAX_TX_SIZE + 1];
            let result = network.publish(TOPIC_TX, oversized);
            assert!(result.is_err());
            assert!(
                result.unwrap_err().to_string().contains("oversized"),
                "error should mention oversized message"
            );

            // Vote over limit
            let oversized_vote = vec![0u8; MAX_VOTE_SIZE + 1];
            let result = network.publish(TOPIC_VOTE, oversized_vote);
            assert!(result.is_err());
            assert!(
                result.unwrap_err().to_string().contains("oversized"),
                "vote error should mention oversized"
            );
        });
    }

    #[test]
    fn test_publish_rejects_unsubscribed_topic() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut network = P2PNetwork::new_random().unwrap();
            let result = network.publish(TOPIC_TX, vec![1, 2, 3]);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("not subscribed"));
        });
    }

    #[test]
    fn test_topic_matching_exact() {
        // Verify that our topic constants don't accidentally match each other
        // when using exact equality (the old `contains()` approach was fragile).
        let topics = [TOPIC_TX, TOPIC_BLOCK, TOPIC_VOTE, TOPIC_SHRED, TOPIC_SYNC];
        for (i, a) in topics.iter().enumerate() {
            for (j, b) in topics.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b, "topics must be distinct");
                }
            }
        }

        // A topic with a similar name must NOT match
        let fake_topic = "/aether/1/tx-extra";
        assert_ne!(fake_topic, TOPIC_TX);
        assert_eq!(max_size_for_topic(fake_topic), MAX_BLOCK_SIZE);
    }
}
