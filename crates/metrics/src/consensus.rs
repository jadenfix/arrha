use once_cell::sync::Lazy;
use prometheus::{register_histogram, register_int_counter, Histogram, IntCounter};

pub struct ConsensusMetrics {
    pub slots_finalized: IntCounter,
    pub fork_events: IntCounter,
    pub finality_latency_ms: Histogram,
    pub blocks_produced: IntCounter,
    pub blocks_received: IntCounter,
    pub consensus_rounds: IntCounter,
    pub transactions_processed: IntCounter,
    pub block_production_ms: Histogram,
}

impl ConsensusMetrics {
    fn new() -> Self {
        ConsensusMetrics {
            slots_finalized: register_int_counter!(
                "aether_consensus_slots_finalized",
                "Number of slots finalized"
            )
            .expect("register slots_finalized"),
            fork_events: register_int_counter!(
                "aether_consensus_fork_events",
                "Observed fork events"
            )
            .expect("register fork_events"),
            finality_latency_ms: register_histogram!(
                "aether_consensus_finality_latency_ms",
                "Latency from block production to finality"
            )
            .expect("register finality latency"),
            blocks_produced: register_int_counter!(
                "aether_consensus_blocks_produced_total",
                "Total blocks produced by this validator"
            )
            .expect("register blocks_produced"),
            blocks_received: register_int_counter!(
                "aether_consensus_blocks_received_total",
                "Total blocks received from peers"
            )
            .expect("register blocks_received"),
            consensus_rounds: register_int_counter!(
                "aether_consensus_rounds_total",
                "Total consensus rounds (slots) processed"
            )
            .expect("register consensus_rounds"),
            transactions_processed: register_int_counter!(
                "aether_consensus_transactions_processed_total",
                "Total transactions processed in blocks"
            )
            .expect("register transactions_processed"),
            block_production_ms: register_histogram!(
                "aether_consensus_block_production_ms",
                "Time to produce a block in milliseconds",
                vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0]
            )
            .expect("register block_production_ms"),
        }
    }
}

pub static CONSENSUS_METRICS: Lazy<ConsensusMetrics> = Lazy::new(ConsensusMetrics::new);
