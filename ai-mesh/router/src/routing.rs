use crate::monitoring::RouterMetrics;
use crate::scoring::{score_provider, ScoreWeights};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRequest {
    pub job_id: String,
    pub required_capabilities: Vec<String>,
    pub min_reputation: i32,
    pub max_latency_ms: u64,
    pub max_price_per_unit: u64,
}

impl Default for JobRequest {
    fn default() -> Self {
        Self {
            job_id: "job-default".to_string(),
            required_capabilities: vec![],
            min_reputation: 0,
            max_latency_ms: 2_000,
            max_price_per_unit: 100_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCandidate {
    pub provider_id: String,
    pub capabilities: Vec<String>,
    pub reputation_score: i32,
    pub avg_latency_ms: u64,
    pub price_per_unit: u64,
    pub available: bool,
    pub active_jobs: u32,
    pub max_concurrent_jobs: u32,
}

impl Default for ProviderCandidate {
    fn default() -> Self {
        Self {
            provider_id: "provider-default".to_string(),
            capabilities: vec![],
            reputation_score: 50,
            avg_latency_ms: 200,
            price_per_unit: 1_000,
            available: true,
            active_jobs: 0,
            max_concurrent_jobs: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingDecision {
    pub job_id: String,
    pub provider_id: String,
    pub score: f64,
}

/// Route a job to the best-scoring provider.
pub fn route_job(job: &JobRequest, providers: &[ProviderCandidate]) -> Option<RoutingDecision> {
    route_job_with_metrics(job, providers, &mut RouterMetrics::new(128))
}

/// Same as `route_job`, with metrics capture for observability.
pub fn route_job_with_metrics(
    job: &JobRequest,
    providers: &[ProviderCandidate],
    metrics: &mut RouterMetrics,
) -> Option<RoutingDecision> {
    let mut ranked: Vec<(&ProviderCandidate, f64)> = providers
        .iter()
        .filter_map(|provider| {
            score_provider(job, provider, ScoreWeights::default()).map(|score| (provider, score))
        })
        .collect();

    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let (provider, score) = ranked.first()?;

    metrics.record(job.job_id.clone(), provider.provider_id.clone(), *score);

    Some(RoutingDecision {
        job_id: job.job_id.clone(),
        provider_id: provider.provider_id.clone(),
        score: *score,
    })
}
