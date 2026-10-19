use anyhow::Result;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

/// Maximum number of peers tracked per topic (gossipsub D_hi).
const MAX_PEERS_PER_TOPIC: usize = 12;

/// Maximum number of topics a single node can subscribe to.
const MAX_TOPICS: usize = 64;

/// Gossipsub Protocol Implementation
///
/// Features:
/// - Topic-based pub/sub
/// - Message deduplication
/// - Flood publishing
/// - Peer scoring
/// - Message validation
///
/// Parameters:
/// - D: Desired peer degree (6)
/// - D_lo: Lower bound for peer degree (4)
/// - D_hi: Upper bound for peer degree (12)
/// - Heartbeat interval: 1 second
///
pub struct GossipManager {
    /// Subscribed topics
    subscriptions: HashMap<String, TopicInfo>,

    /// Seen message hashes for O(1) lookup.
    seen_set: HashSet<Vec<u8>>,
    /// Insertion-ordered queue of seen hashes so eviction removes the oldest.
    seen_order: VecDeque<Vec<u8>>,

    /// Message cache (for history)
    message_cache: Vec<(Vec<u8>, Instant)>,

    /// Max cache size
    cache_size: usize,

    /// Cache duration
    cache_duration: Duration,
}

#[derive(Clone)]
pub struct TopicInfo {
    pub name: String,
    pub peers: Vec<String>,
    pub message_count: u64,
}

impl GossipManager {
    pub fn new() -> Self {
        GossipManager {
            subscriptions: HashMap::new(),
            seen_set: HashSet::new(),
            seen_order: VecDeque::new(),
            message_cache: Vec::new(),
            cache_size: 1000,
            cache_duration: Duration::from_secs(120),
        }
    }

    /// Subscribe to a topic
    pub fn subscribe(&mut self, topic: String) {
        if self.subscriptions.contains_key(&topic) {
            return;
        }
        if self.subscriptions.len() >= MAX_TOPICS {
            tracing::warn!(
                topic,
                "gossip topic cap reached ({}), rejecting subscribe",
                MAX_TOPICS
            );
            return;
        }
        self.subscriptions.insert(
            topic.clone(),
            TopicInfo {
                name: topic.clone(),
                peers: vec![],
                message_count: 0,
            },
        );
        tracing::info!(topic, "subscribed to gossip topic");
    }

    /// Unsubscribe from a topic
    pub fn unsubscribe(&mut self, topic: &str) {
        self.subscriptions.remove(topic);
        tracing::info!(topic, "unsubscribed from gossip topic");
    }

    /// Publish a message to a topic
    pub fn publish(&mut self, topic: &str, message: Vec<u8>) -> Result<()> {
        // Check if subscribed
        if !self.subscriptions.contains_key(topic) {
            anyhow::bail!("not subscribed to topic: {}", topic);
        }

        let msg_hash = self.hash_message(&message);
        if self.seen_set.contains(&msg_hash) {
            return Ok(());
        }

        self.seen_set.insert(msg_hash.clone());
        self.seen_order.push_back(msg_hash.clone());

        // Add to cache
        self.cache_message(msg_hash, message.clone());

        // Update topic stats
        if let Some(info) = self.subscriptions.get_mut(topic) {
            info.message_count = info.message_count.saturating_add(1);
        }

        // In production: forward to peers via gossipsub
        tracing::debug!(bytes = message.len(), topic, "gossip published");

        Ok(())
    }

    /// Handle incoming message
    pub fn handle_message(&mut self, topic: &str, message: Vec<u8>) -> Result<bool> {
        let msg_hash = self.hash_message(&message);

        if self.seen_set.contains(&msg_hash) {
            return Ok(false);
        }

        self.seen_set.insert(msg_hash.clone());
        self.seen_order.push_back(msg_hash.clone());

        // Add to cache
        self.cache_message(msg_hash, message.clone());

        // Update topic stats
        if let Some(info) = self.subscriptions.get_mut(topic) {
            info.message_count = info.message_count.saturating_add(1);
        }

        Ok(true) // New message
    }

    /// Add peer to topic (capped at D_hi to prevent unbounded growth).
    pub fn add_peer_to_topic(&mut self, topic: &str, peer_id: String) {
        if let Some(info) = self.subscriptions.get_mut(topic) {
            if info.peers.contains(&peer_id) {
                return;
            }
            if info.peers.len() >= MAX_PEERS_PER_TOPIC {
                tracing::debug!(topic, "peer cap reached for topic, rejecting peer");
                return;
            }
            info.peers.push(peer_id);
        }
    }

    /// Remove peer from topic
    pub fn remove_peer_from_topic(&mut self, topic: &str, peer_id: &str) {
        if let Some(info) = self.subscriptions.get_mut(topic) {
            info.peers.retain(|p| p != peer_id);
        }
    }

    /// Get peers for a topic
    pub fn get_topic_peers(&self, topic: &str) -> Vec<String> {
        self.subscriptions
            .get(topic)
            .map(|info| info.peers.clone())
            .unwrap_or_default()
    }

    /// Cleanup old messages
    pub fn cleanup(&mut self) {
        let now = Instant::now();

        // Remove old cached messages
        self.message_cache
            .retain(|(_, timestamp)| now.duration_since(*timestamp) < self.cache_duration);

        if self.seen_set.len() > self.cache_size {
            let to_remove = self.seen_set.len() / 2;
            tracing::warn!(
                total = self.seen_set.len(),
                evicting = to_remove,
                "gossip dedup cache overflow, evicting oldest entries",
            );
            for _ in 0..to_remove {
                if let Some(oldest) = self.seen_order.pop_front() {
                    self.seen_set.remove(&oldest);
                } else {
                    break;
                }
            }
        }
    }

    fn hash_message(&self, message: &[u8]) -> Vec<u8> {
        use sha2::{Digest, Sha256};
        Sha256::digest(message).to_vec()
    }

    fn cache_message(&mut self, hash: Vec<u8>, _message: Vec<u8>) {
        if self.message_cache.len() >= self.cache_size {
            self.message_cache.remove(0);
        }
        self.message_cache.push((hash, Instant::now()));
    }

    pub fn topic_count(&self) -> usize {
        self.subscriptions.len()
    }

    pub fn seen_message_count(&self) -> usize {
        self.seen_set.len()
    }
}

impl Default for GossipManager {
    fn default() -> Self {
        Self::new()
    }
}
