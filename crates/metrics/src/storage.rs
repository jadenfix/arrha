use once_cell::sync::Lazy;
use prometheus::{register_histogram, register_int_counter, Histogram, IntCounter};

pub struct StorageMetrics {
    pub write_batch_ms: Histogram,
    pub read_latency_ms: Histogram,
    pub blocks_persisted: IntCounter,
    pub bytes_written: IntCounter,
}

impl StorageMetrics {
    fn new() -> Self {
        StorageMetrics {
            write_batch_ms: register_histogram!(
                "aether_storage_write_batch_ms",
                "RocksDB write batch latency in milliseconds",
                vec![0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0]
            )
            .expect("register write_batch_ms"),
            read_latency_ms: register_histogram!(
                "aether_storage_read_latency_ms",
                "RocksDB read latency in milliseconds",
                vec![0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0]
            )
            .expect("register read_latency_ms"),
            blocks_persisted: register_int_counter!(
                "aether_storage_blocks_persisted_total",
                "Total blocks persisted to disk"
            )
            .expect("register blocks_persisted"),
            bytes_written: register_int_counter!(
                "aether_storage_bytes_written_total",
                "Total bytes written to storage"
            )
            .expect("register bytes_written"),
        }
    }
}

pub static STORAGE_METRICS: Lazy<StorageMetrics> = Lazy::new(StorageMetrics::new);
