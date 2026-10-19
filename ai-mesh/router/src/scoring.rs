use crate::routing::{JobRequest, ProviderCandidate};

#[derive(Debug, Clone, Copy)]
pub struct ScoreWeights {
    pub reputation: f64,
    pub latency: f64,
    pub price: f64,
}

impl Default for ScoreWeights {
    fn default() -> Self {
        Self {
            reputation: 0.5,
            latency: 0.3,
            price: 0.2,
        }
    }
}

pub fn score_provider(
    job: &JobRequest,
    provider: &ProviderCandidate,
    weights: ScoreWeights,
) -> Option<f64> {
    if !provider.available {
        return None;
    }
    if provider.reputation_score < job.min_reputation {
        return None;
    }
    if provider.avg_latency_ms > job.max_latency_ms {
        return None;
    }
    if provider.price_per_unit > job.max_price_per_unit {
        return None;
    }

    if !job
        .required_capabilities
        .iter()
        .all(|cap| provider.capabilities.contains(cap))
    {
        return None;
    }

    let normalized_rep = (provider.reputation_score as f64 / 100.0).clamp(0.0, 1.0);
    let latency_ratio = provider.avg_latency_ms as f64 / job.max_latency_ms as f64;
    let normalized_latency = (1.0 - latency_ratio).clamp(0.0, 1.0);
    let price_ratio = provider.price_per_unit as f64 / job.max_price_per_unit as f64;
    let normalized_price = (1.0 - price_ratio).clamp(0.0, 1.0);

    let load_ratio = if provider.max_concurrent_jobs == 0 {
        1.0
    } else {
        provider.active_jobs as f64 / provider.max_concurrent_jobs as f64
    };
    let load_penalty = (1.0 - 0.5 * load_ratio).clamp(0.0, 1.0);

    let weighted = normalized_rep * weights.reputation
        + normalized_latency * weights.latency
        + normalized_price * weights.price;
    Some(weighted * load_penalty)
}
