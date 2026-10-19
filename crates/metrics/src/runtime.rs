use once_cell::sync::Lazy;
use prometheus::{register_histogram, register_int_counter, Histogram, IntCounter};

pub struct RuntimeMetrics {
    pub tx_executed: IntCounter,
    pub execution_time_ms: Histogram,
    pub parallel_speedup: Histogram,
}

impl RuntimeMetrics {
    fn new() -> Self {
        RuntimeMetrics {
            tx_executed: register_int_counter!(
                "aether_runtime_tx_executed",
                "Transactions executed by the runtime"
            )
            .expect("register tx_executed"),
            execution_time_ms: register_histogram!(
                "aether_runtime_execution_ms",
                "Transaction execution time in milliseconds"
            )
            .expect("register execution_time"),
            parallel_speedup: register_histogram!(
                "aether_runtime_parallel_speedup",
                "Effective parallel execution speedup"
            )
            .expect("register speedup"),
        }
    }
}

pub static RUNTIME_METRICS: Lazy<RuntimeMetrics> = Lazy::new(RuntimeMetrics::new);
