use once_cell::sync::Lazy;
use prometheus::{register_int_counter, register_int_counter_vec, IntCounter, IntCounterVec};

/// Per-topic gossipsub metrics for production observability.
///
/// Operators need to distinguish tx/block/vote/shred/sync message rates
/// to detect spam, monitor consensus health, and tune gossipsub parameters.
pub struct P2PMetrics {
    /// Messages received per topic (tx, block, vote, shred, sync).
    pub messages_received_by_topic: IntCounterVec,
    /// Messages dropped due to oversized payload, per topic.
    pub messages_dropped_oversized: IntCounterVec,
    /// Messages dropped from banned peers.
    pub messages_dropped_banned: IntCounter,
    /// Messages dropped due to per-peer rate limiting.
    pub messages_dropped_rate_limited: IntCounter,
    /// Peers banned (score below threshold).
    pub peers_banned: IntCounter,
}

impl P2PMetrics {
    fn new() -> Self {
        P2PMetrics {
            messages_received_by_topic: register_int_counter_vec!(
                "aether_p2p_messages_received_by_topic_total",
                "Gossipsub messages received, labeled by topic",
                &["topic"]
            )
            .expect("register messages_received_by_topic"),
            messages_dropped_oversized: register_int_counter_vec!(
                "aether_p2p_messages_dropped_oversized_total",
                "Messages dropped due to exceeding per-topic size limit",
                &["topic"]
            )
            .expect("register messages_dropped_oversized"),
            messages_dropped_rate_limited: register_int_counter!(
                "aether_p2p_messages_dropped_rate_limited_total",
                "Messages dropped due to per-peer rate limiting"
            )
            .expect("register messages_dropped_rate_limited"),
            messages_dropped_banned: register_int_counter!(
                "aether_p2p_messages_dropped_banned_total",
                "Messages dropped from banned peers"
            )
            .expect("register messages_dropped_banned"),
            peers_banned: register_int_counter!(
                "aether_p2p_peers_banned_total",
                "Total peers banned due to low reputation score"
            )
            .expect("register peers_banned"),
        }
    }
}

pub static P2P_METRICS: Lazy<P2PMetrics> = Lazy::new(P2PMetrics::new);

/// Canonical short topic labels for Prometheus (avoids high-cardinality full paths).
pub fn topic_label(topic: &str) -> &'static str {
    if topic.contains("/tx") {
        "tx"
    } else if topic.contains("/block") {
        "block"
    } else if topic.contains("/vote") {
        "vote"
    } else if topic.contains("/shred") {
        "shred"
    } else if topic.contains("/sync") {
        "sync"
    } else {
        "unknown"
    }
}
