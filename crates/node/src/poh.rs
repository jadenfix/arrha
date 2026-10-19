use blake3::Hasher;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const MAX_SAMPLES: usize = 128;

#[derive(Debug, Clone)]
pub struct PohMetrics {
    pub tick_count: usize,
    pub last_duration_ms: f64,
    pub average_duration_ms: f64,
    pub jitter_ms: f64,
    pub hash: [u8; 32],
}

pub struct PohRecorder {
    last_hash: [u8; 32],
    last_tick: Instant,
    durations: VecDeque<Duration>,
    tick_count: usize,
}

impl Default for PohRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl PohRecorder {
    pub fn new() -> Self {
        Self::with_start(Instant::now())
    }

    pub fn with_start(start: Instant) -> Self {
        PohRecorder {
            last_hash: *Hasher::new().finalize().as_bytes(),
            last_tick: start,
            durations: VecDeque::new(),
            tick_count: 0,
        }
    }

    pub fn tick(&mut self, now: Instant) -> PohMetrics {
        let duration = now.saturating_duration_since(self.last_tick);
        self.last_tick = now;
        self.tick_count = self.tick_count.wrapping_add(1);

        if self.durations.len() == MAX_SAMPLES {
            self.durations.pop_front();
        }
        self.durations.push_back(duration);

        let mut hasher = Hasher::new();
        hasher.update(&self.last_hash);
        hasher.update(&duration.as_nanos().to_le_bytes());
        self.last_hash = *hasher.finalize().as_bytes();

        let last_duration_ms = duration.as_secs_f64() * 1_000.0;
        let (avg_ms, jitter_ms) = compute_stats(&self.durations);

        PohMetrics {
            tick_count: self.tick_count,
            last_duration_ms,
            average_duration_ms: avg_ms,
            jitter_ms,
            hash: self.last_hash,
        }
    }
}

fn compute_stats(samples: &VecDeque<Duration>) -> (f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }

    let values: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1_000.0).collect();
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance = values
        .iter()
        .map(|value| {
            let diff = value - mean;
            diff * diff
        })
        .sum::<f64>()
        / values.len() as f64;
    let jitter = variance.sqrt();

    (mean, jitter)
}
