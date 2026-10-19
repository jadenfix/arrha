use once_cell::sync::Lazy;
use prometheus::{register_int_counter, register_int_gauge, IntCounter, IntGauge};

pub struct MempoolMetrics {
    /// Current number of transactions in the mempool (pending + queued).
    pub pool_size: IntGauge,
    /// Current number of pending (ready-to-execute) transactions.
    pub pending_size: IntGauge,
    /// Current number of queued (future-nonce) transactions.
    pub queued_size: IntGauge,
    /// Total transactions admitted to the mempool.
    pub admitted_total: IntCounter,
    /// Total transactions evicted due to capacity limits.
    pub evictions_total: IntCounter,
    /// Total transactions rejected due to per-sender rate limiting.
    pub rate_limited_total: IntCounter,
    /// Total transactions rejected (all reasons: duplicate, low nonce, bad sig, etc.).
    pub rejected_total: IntCounter,
    /// Total transactions removed after block inclusion.
    pub removed_total: IntCounter,
    /// Total replace-by-fee replacements.
    pub rbf_replacements_total: IntCounter,
    /// Total reorg events processed.
    pub reorgs_total: IntCounter,
}

impl MempoolMetrics {
    fn new() -> Self {
        MempoolMetrics {
            pool_size: register_int_gauge!(
                "aether_mempool_size",
                "Current number of transactions in the mempool"
            )
            .expect("register mempool pool_size"),

            pending_size: register_int_gauge!(
                "aether_mempool_pending_size",
                "Current number of pending (ready-to-execute) transactions"
            )
            .expect("register mempool pending_size"),

            queued_size: register_int_gauge!(
                "aether_mempool_queued_size",
                "Current number of queued (future-nonce) transactions"
            )
            .expect("register mempool queued_size"),

            admitted_total: register_int_counter!(
                "aether_mempool_admitted_total",
                "Total transactions admitted to the mempool"
            )
            .expect("register mempool admitted_total"),

            evictions_total: register_int_counter!(
                "aether_mempool_evictions_total",
                "Total transactions evicted due to capacity limits"
            )
            .expect("register mempool evictions_total"),

            rate_limited_total: register_int_counter!(
                "aether_mempool_rate_limited_total",
                "Total transactions rejected due to per-sender rate limiting"
            )
            .expect("register mempool rate_limited_total"),

            rejected_total: register_int_counter!(
                "aether_mempool_rejected_total",
                "Total transactions rejected (all reasons)"
            )
            .expect("register mempool rejected_total"),

            removed_total: register_int_counter!(
                "aether_mempool_removed_total",
                "Total transactions removed after block inclusion"
            )
            .expect("register mempool removed_total"),

            rbf_replacements_total: register_int_counter!(
                "aether_mempool_rbf_replacements_total",
                "Total replace-by-fee transaction replacements"
            )
            .expect("register mempool rbf_replacements_total"),

            reorgs_total: register_int_counter!(
                "aether_mempool_reorgs_total",
                "Total reorg events processed by the mempool"
            )
            .expect("register mempool reorgs_total"),
        }
    }
}

pub static MEMPOOL_METRICS: Lazy<MempoolMetrics> = Lazy::new(MempoolMetrics::new);
