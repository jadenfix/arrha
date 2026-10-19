// Data Availability Metrics
use once_cell::sync::Lazy;
use prometheus::{
    register_histogram, register_int_counter, register_int_gauge, Histogram, IntCounter, IntGauge,
};

pub struct DAMetrics {
    // Turbine metrics
    pub shreds_broadcasted: IntCounter,
    pub shreds_received: IntCounter,
    pub blocks_reconstructed: IntCounter,
    pub reconstruction_failures: IntCounter,
    pub reconstruction_latency_ms: Histogram,

    // Erasure coding metrics
    pub encoding_latency_ms: Histogram,
    pub decoding_latency_ms: Histogram,
    pub encoding_throughput_mbps: Histogram,
    pub decoding_throughput_mbps: Histogram,

    // Packet loss metrics
    pub packets_lost: IntCounter,
    pub packets_recovered: IntCounter,
    pub loss_rate: Histogram,

    // Current state gauges
    pub pending_reconstructions: IntGauge,
    pub shred_cache_size: IntGauge,
}

impl DAMetrics {
    fn new() -> Self {
        DAMetrics {
            shreds_broadcasted: register_int_counter!(
                "aether_da_shreds_broadcasted_total",
                "Total shreds broadcasted by this node"
            )
            .expect("register shreds_broadcasted"),

            shreds_received: register_int_counter!(
                "aether_da_shreds_received_total",
                "Total shreds received by this node"
            )
            .expect("register shreds_received"),

            blocks_reconstructed: register_int_counter!(
                "aether_da_blocks_reconstructed_total",
                "Total blocks successfully reconstructed"
            )
            .expect("register blocks_reconstructed"),

            reconstruction_failures: register_int_counter!(
                "aether_da_reconstruction_failures_total",
                "Total block reconstruction failures"
            )
            .expect("register reconstruction_failures"),

            reconstruction_latency_ms: register_histogram!(
                "aether_da_reconstruction_latency_ms",
                "Block reconstruction latency in milliseconds",
                vec![1.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0]
            )
            .expect("register reconstruction_latency"),

            encoding_latency_ms: register_histogram!(
                "aether_da_encoding_latency_ms",
                "Erasure coding encoding latency in milliseconds",
                vec![1.0, 5.0, 10.0, 20.0, 50.0, 100.0]
            )
            .expect("register encoding_latency"),

            decoding_latency_ms: register_histogram!(
                "aether_da_decoding_latency_ms",
                "Erasure coding decoding latency in milliseconds",
                vec![1.0, 5.0, 10.0, 20.0, 50.0, 100.0]
            )
            .expect("register decoding_latency"),

            encoding_throughput_mbps: register_histogram!(
                "aether_da_encoding_throughput_mbps",
                "Erasure coding encoding throughput in MB/s",
                vec![10.0, 50.0, 100.0, 200.0, 500.0, 1000.0]
            )
            .expect("register encoding_throughput"),

            decoding_throughput_mbps: register_histogram!(
                "aether_da_decoding_throughput_mbps",
                "Erasure coding decoding throughput in MB/s",
                vec![10.0, 50.0, 100.0, 200.0, 500.0, 1000.0]
            )
            .expect("register decoding_throughput"),

            packets_lost: register_int_counter!(
                "aether_da_packets_lost_total",
                "Total packets/shreds lost"
            )
            .expect("register packets_lost"),

            packets_recovered: register_int_counter!(
                "aether_da_packets_recovered_total",
                "Total packets recovered via erasure coding"
            )
            .expect("register packets_recovered"),

            loss_rate: register_histogram!(
                "aether_da_loss_rate",
                "Packet loss rate (0.0-1.0)",
                vec![0.0, 0.01, 0.05, 0.10, 0.20, 0.50]
            )
            .expect("register loss_rate"),

            pending_reconstructions: register_int_gauge!(
                "aether_da_pending_reconstructions",
                "Number of blocks currently being reconstructed"
            )
            .expect("register pending_reconstructions"),

            shred_cache_size: register_int_gauge!(
                "aether_da_shred_cache_size",
                "Current size of shred cache"
            )
            .expect("register shred_cache_size"),
        }
    }
}

pub static DA_METRICS: Lazy<DAMetrics> = Lazy::new(DAMetrics::new);
