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
use std::collections::{HashMap, HashSet};
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
/// Keep an address-only dial marked in flight long enough for libp2p's
/// transport and handshake timeouts, preventing retry ticks from duplicating it.
const ADDRESS_DIAL_TIMEOUT: Duration = Duration::from_secs(30);

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

async fn resolve_dns_multiaddr(addr: &Multiaddr, original: &str) -> Result<Option<Multiaddr>> {
    let dns_target = {
        let protocols: Vec<_> = addr.iter().collect();
        if protocols
            .iter()
            .any(|protocol| matches!(protocol, libp2p::multiaddr::Protocol::Dnsaddr(_)))
        {
            anyhow::bail!("dnsaddr bootstrap peers are unsupported: {original}");
        }

        let dns_host = match protocols.first() {
            Some(libp2p::multiaddr::Protocol::Dns4(host)) => Some(((*host).clone(), true)),
            Some(libp2p::multiaddr::Protocol::Dns6(host)) => Some(((*host).clone(), false)),
            _ => None,
        };
        dns_host
            .map(|(host, require_ipv4)| {
                let port = match protocols.get(1) {
                    Some(libp2p::multiaddr::Protocol::Tcp(port)) => *port,
                    _ => {
                        anyhow::bail!("DNS bootstrap address must use /dns4|dns6/<host>/tcp/<port>: {original}")
                    }
                };
                let peer_suffix = match protocols.as_slice() {
                    [_, _, libp2p::multiaddr::Protocol::P2p(peer_id)] => {
                        format!("/p2p/{peer_id}")
                    }
                    [_, _] => String::new(),
                    _ => anyhow::bail!("unsupported DNS bootstrap address protocols: {original}"),
                };
                Ok::<_, anyhow::Error>((host, require_ipv4, port, peer_suffix))
            })
            .transpose()?
    };

    let Some((host, require_ipv4, port, peer_suffix)) = dns_target else {
        return Ok(None);
    };
    let mut lookup = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::lookup_host((&host[..], port)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("DNS lookup timed out for {host}"))??;
    let resolved = lookup
        .find(|address| address.is_ipv4() == require_ipv4)
        .ok_or_else(|| anyhow::anyhow!("DNS lookup returned no compatible addresses for {host}"))?;
    let host = match resolved.ip() {
        std::net::IpAddr::V4(ip) => format!("/ip4/{ip}/tcp/{port}"),
        std::net::IpAddr::V6(ip) => format!("/ip6/{ip}/tcp/{port}"),
    };
    let resolved_addr = format!("{host}{peer_suffix}")
        .parse()
        .map_err(|e| anyhow::anyhow!("failed to build resolved peer address: {e}"))?;
    Ok(Some(resolved_addr))
}

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
    /// Peer-ID-addressed dials in flight; retries must not create parallel dials.
    dialing_peers: HashSet<PeerId>,
    /// Address-only bootstrap dials (common in container DNS setups).
    dialing_addresses: HashMap<String, (Multiaddr, Instant)>,
    /// Resolved bootstrap target identities, used to skip DNS while connected.
    bootstrap_target_peers: HashMap<String, PeerId>,
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
            dialing_peers: HashSet::new(),
            dialing_addresses: HashMap::new(),
            bootstrap_target_peers: HashMap::new(),
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

    /// Connect to a peer by multiaddr. DNS4/DNS6 hosts are resolved through
    /// the operating system with a bounded lookup before dialing. Refuses to
    /// dial banned peers and explicitly rejects DNSADDR.
    pub async fn connect_peer(&mut self, addr: &str) -> Result<()> {
        let original: Multiaddr = addr
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid multiaddr: {}", e))?;
        if let Some(peer_id) = self.bootstrap_target_peers.get(addr).copied() {
            if self.is_banned(&peer_id) {
                return Err(anyhow::anyhow!(
                    "peer {} is banned, refusing to dial",
                    peer_id
                ));
            }
            if self.peers.contains_key(&peer_id) || self.swarm.is_connected(&peer_id) {
                return Ok(());
            }
            self.bootstrap_target_peers.remove(addr);
        }
        if let Some(libp2p::multiaddr::Protocol::P2p(peer_id)) = original.iter().last() {
            if self.is_banned(&peer_id) {
                return Err(anyhow::anyhow!(
                    "peer {} is banned, refusing to dial",
                    peer_id
                ));
            }
            if self.peers.contains_key(&peer_id)
                || self.swarm.is_connected(&peer_id)
                || self.dialing_peers.contains(&peer_id)
            {
                return Ok(());
            }
        }
        let now = Instant::now();
        self.dialing_addresses
            .retain(|_, (_, started)| now.duration_since(*started) < ADDRESS_DIAL_TIMEOUT);
        if self
            .dialing_addresses
            .get(addr)
            .is_some_and(|(_, started)| now.duration_since(*started) < ADDRESS_DIAL_TIMEOUT)
        {
            return Ok(());
        }

        let multiaddr = resolve_dns_multiaddr(&original, addr)
            .await?
            .unwrap_or(original);

        // Extract peer ID from multiaddr and reject if banned
        let peer_id = match multiaddr.iter().last() {
            Some(libp2p::multiaddr::Protocol::P2p(peer_id)) => Some(peer_id),
            _ => None,
        };
        if let Some(peer_id) = peer_id {
            if self.is_banned(&peer_id) {
                return Err(anyhow::anyhow!(
                    "peer {} is banned, refusing to dial",
                    peer_id
                ));
            }
            if self.peers.contains_key(&peer_id)
                || self.swarm.is_connected(&peer_id)
                || self.dialing_peers.contains(&peer_id)
            {
                return Ok(());
            }
        } else {
            let transport_addr = without_peer_id(&multiaddr);
            if self.peers.values().any(|peer| {
                peer.address
                    .parse::<Multiaddr>()
                    .is_ok_and(|address| without_peer_id(&address) == transport_addr)
            }) {
                return Ok(());
            }
        }

        self.swarm.dial(multiaddr.clone())?;
        if let Some(peer_id) = peer_id {
            self.dialing_peers.insert(peer_id);
        } else {
            self.dialing_addresses.insert(
                addr.to_owned(),
                (without_peer_id(&multiaddr), Instant::now()),
            );
        }
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
                SwarmEvent::ConnectionEstablished {
                    peer_id, endpoint, ..
                } => {
                    self.dialing_peers.remove(&peer_id);
                    let remote_addr = endpoint.get_remote_address().clone();
                    let remote_transport_addr = without_peer_id(&remote_addr);
                    let matching_dial_addr = self
                        .dialing_addresses
                        .iter()
                        .find(|(_, (address, _))| without_peer_id(address) == remote_transport_addr)
                        .map(|(key, (address, _))| (key.clone(), address.clone()));
                    if let Some((key, _)) = &matching_dial_addr {
                        self.dialing_addresses.remove(key);
                        self.bootstrap_target_peers.insert(key.clone(), peer_id);
                    }
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

                    if let Some(info) = self.peers.get_mut(&peer_id) {
                        // A duplicate inbound connection can report the
                        // remote ephemeral port. Preserve the actual bootstrap
                        // target learned from an outbound dial when available.
                        if let Some((_, dial_addr)) = matching_dial_addr {
                            info.address = dial_addr.to_string();
                        }
                        continue;
                    }

                    let info = PeerInfo {
                        id: peer_id.to_string(),
                        address: matching_dial_addr
                            .map(|(_, dial_addr)| dial_addr.to_string())
                            .unwrap_or_else(|| remote_addr.to_string()),
                        score: 0,
                        connected_at: current_timestamp(),
                    };
                    self.peers.insert(peer_id, info);
                    NET_METRICS.connections_total.inc();
                    NET_METRICS.peers_connected.set(self.peers.len() as i64);
                    return Some(NetworkEvent::PeerConnected(peer_id));
                }
                SwarmEvent::ConnectionClosed {
                    peer_id,
                    num_established,
                    ..
                } => {
                    if num_established == 0 {
                        self.peers.remove(&peer_id);
                        self.rate_limiters.remove(&peer_id);
                        self.bootstrap_target_peers
                            .retain(|_, target_peer| *target_peer != peer_id);
                        NET_METRICS.peers_connected.set(self.peers.len() as i64);
                        return Some(NetworkEvent::PeerDisconnected(peer_id));
                    }
                    continue;
                }
                SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                    if let Some(peer_id) = peer_id {
                        self.dialing_peers.remove(&peer_id);
                    }
                    tracing::debug!(?peer_id, %error, "outbound peer dial failed");
                    continue;
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

fn without_peer_id(addr: &Multiaddr) -> Multiaddr {
    let mut transport_addr = addr.clone();
    if matches!(
        transport_addr.iter().last(),
        Some(libp2p::multiaddr::Protocol::P2p(_))
    ) {
        transport_addr.pop();
    }
    transport_addr
}

fn current_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
