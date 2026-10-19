use once_cell::sync::Lazy;
use prometheus::{register_int_counter, register_int_gauge, IntCounter, IntGauge};

pub struct NodeMetrics {
    /// 1 if the node is currently syncing, 0 if synced.
    pub sync_active: IntGauge,
    /// How many slots behind the network tip this node is.
    pub sync_slot_lag: IntGauge,
    /// Total blocks successfully applied during sync sessions.
    pub sync_blocks_applied: IntCounter,
    /// Number of times sync has stalled and been retried.
    pub sync_stalls: IntCounter,
    /// Current slot height of this node.
    pub current_slot: IntGauge,
    /// Number of blocks currently buffered waiting for sync ordering.
    pub sync_buffer_size: IntGauge,
}

impl NodeMetrics {
    fn new() -> Self {
        NodeMetrics {
            sync_active: register_int_gauge!(
                "aether_node_sync_active",
                "Whether the node is currently syncing (1) or synced (0)"
            )
            .expect("register sync_active"),
            sync_slot_lag: register_int_gauge!(
                "aether_node_sync_slot_lag",
                "Number of slots behind the network tip"
            )
            .expect("register sync_slot_lag"),
            sync_blocks_applied: register_int_counter!(
                "aether_node_sync_blocks_applied_total",
                "Total blocks applied during sync sessions"
            )
            .expect("register sync_blocks_applied"),
            sync_stalls: register_int_counter!(
                "aether_node_sync_stalls_total",
                "Number of sync stall events"
            )
            .expect("register sync_stalls"),
            current_slot: register_int_gauge!(
                "aether_node_current_slot",
                "Current slot height of this node"
            )
            .expect("register current_slot"),
            sync_buffer_size: register_int_gauge!(
                "aether_node_sync_buffer_size",
                "Number of blocks buffered during sync"
            )
            .expect("register sync_buffer_size"),
        }
    }
}

pub static NODE_METRICS: Lazy<NodeMetrics> = Lazy::new(NodeMetrics::new);
