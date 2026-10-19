use std::collections::HashSet;

use aether_types::{Address, Slot, H256};
use serde::{Deserialize, Serialize};

use crate::ewma::Ewma;

const SUCCESS_WEIGHT: f64 = 0.4;
const LATENCY_WEIGHT: f64 = 0.3;
const UPTIME_WEIGHT: f64 = 0.2;
const DISPUTE_LOSS_PENALTY: f64 = 15.0;
const DISPUTE_WIN_BONUS: f64 = 2.0;
const MAX_SCORE: f64 = 100.0;
const MAX_LATENCY_MS: f64 = 30_000.0;
const SCORE_MIN: f64 = 0.0;
const ALPHA: f64 = 0.95;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum HardwareTier {
    Standard,
    Premium,
    Dedicated,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderReputation {
    pub address: Address,
    pub score: f64,
    pub jobs_completed: u64,
    pub jobs_failed: u64,
    pub disputes_won: u32,
    pub disputes_lost: u32,
    pub last_active_slot: Slot,
    pub hardware_tier: HardwareTier,
    pub supported_models: HashSet<H256>,
    latency_ewma: Ewma,
    uptime_ewma: Ewma,
}

impl ProviderReputation {
    pub fn new(address: Address, tier: HardwareTier) -> Self {
        ProviderReputation {
            address,
            score: 50.0,
            jobs_completed: 0,
            jobs_failed: 0,
            disputes_won: 0,
            disputes_lost: 0,
            last_active_slot: 0,
            hardware_tier: tier,
            supported_models: HashSet::new(),
            latency_ewma: Ewma::new(ALPHA),
            uptime_ewma: Ewma::new(ALPHA),
        }
    }

    pub fn add_model(&mut self, model: H256) {
        self.supported_models.insert(model);
    }

    pub fn record_job_success(&mut self, latency_ms: f64, uptime_ratio: f64, slot: Slot) {
        self.jobs_completed += 1;
        self.latency_ewma.update(latency_ms);
        self.uptime_ewma.update(uptime_ratio);
        self.last_active_slot = slot;
        self.recompute_score();
    }

    pub fn record_job_failure(&mut self, slot: Slot) {
        self.jobs_failed += 1;
        self.score *= 0.9;
        self.last_active_slot = slot;
        self.score = self.score.clamp(SCORE_MIN, MAX_SCORE);
    }

    pub fn record_dispute(&mut self, won: bool) {
        if won {
            self.disputes_won += 1;
        } else {
            self.disputes_lost += 1;
        }
        self.recompute_score();
    }

    pub fn uptime(&self) -> f64 {
        self.uptime_ewma.value()
    }

    pub fn avg_latency(&self) -> f64 {
        self.latency_ewma.value()
    }

    fn recompute_score(&mut self) {
        let total_jobs = self.jobs_completed + self.jobs_failed;

        // A provider with no job history uses a neutral success_rate of 0.5 so that
        // recompute_score() is idempotent from the initial state.  Without this guard,
        // recompute_score() would use success_rate=0.0 and produce 30.0, which is
        // inconsistent with the starting score of 50.0.
        let success_rate = if total_jobs == 0 {
            0.5
        } else {
            self.jobs_completed as f64 / total_jobs as f64
        };

        let latency_score = if self.latency_ewma.initialized() {
            1.0 - (self.latency_ewma.value() / MAX_LATENCY_MS).min(1.0)
        } else {
            1.0
        };

        let uptime_score = self.uptime_ewma.value().clamp(0.0, 1.0);

        let mut score = SUCCESS_WEIGHT * success_rate * MAX_SCORE
            + LATENCY_WEIGHT * latency_score * MAX_SCORE
            + UPTIME_WEIGHT * uptime_score * MAX_SCORE;

        score -= self.disputes_lost as f64 * DISPUTE_LOSS_PENALTY;
        score += self.disputes_won as f64 * DISPUTE_WIN_BONUS;

        self.score = score.clamp(SCORE_MIN, MAX_SCORE);
    }
}
