use std::cmp::Ordering;

use aether_types::{Address, Slot, H256};

use crate::scoring::{HardwareTier, ProviderReputation};

pub fn top_providers(
    providers: &[ProviderReputation],
    model: H256,
    minimum_score: f64,
    tier: HardwareTier,
    current_slot: Slot,
    staleness_threshold: Slot,
    limit: usize,
) -> Vec<&ProviderReputation> {
    let mut candidates: Vec<&ProviderReputation> = providers
        .iter()
        .filter(|provider| provider.score >= minimum_score)
        .filter(|provider| provider.hardware_tier >= tier)
        .filter(|provider| provider.supported_models.contains(&model))
        .filter(|provider| {
            current_slot.saturating_sub(provider.last_active_slot) <= staleness_threshold
        })
        .collect();

    candidates.sort_by(|a, b| {
        b.score.partial_cmp(&a.score).unwrap_or_else(|| {
            // Treat NaN as worst (lowest score)
            if b.score.is_nan() {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        })
    });
    candidates.truncate(limit);
    candidates
}

pub fn provider_addresses(providers: &[&ProviderReputation]) -> Vec<Address> {
    providers.iter().map(|provider| provider.address).collect()
}
