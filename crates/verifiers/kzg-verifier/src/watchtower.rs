use rand::{rngs::StdRng, seq::IteratorRandom, SeedableRng};

use crate::challenge::KzgChallenge;
use aether_types::H256;

/// Build a random challenge selecting a subset of layers and evaluation points.
///
/// The function is deterministic when a seed is provided which keeps tests stable.
pub fn build_challenge(
    vcr_id: H256,
    total_layers: u32,
    layer_size: u32,
    sample_layers: usize,
    sample_points: usize,
    seed: Option<u64>,
) -> anyhow::Result<KzgChallenge> {
    anyhow::ensure!(total_layers > 0, "total layers must be positive");
    anyhow::ensure!(layer_size > 0, "layer size must be positive");
    anyhow::ensure!(sample_layers > 0, "must request at least one layer");
    anyhow::ensure!(
        sample_points > 0,
        "must request at least one point per layer"
    );

    let mut rng = match seed {
        Some(s) => StdRng::seed_from_u64(s),
        None => StdRng::from_rng(rand::thread_rng())
            .map_err(|e| anyhow::anyhow!("RNG initialization failed: {e}"))?,
    };

    let mut layers: Vec<u32> = (0..total_layers).collect();
    layers.sort();

    let selected_layers: Vec<u32> = layers
        .into_iter()
        .choose_multiple(&mut rng, sample_layers.min(total_layers as usize));

    let mut point_indices = Vec::with_capacity(selected_layers.len());
    for _layer in &selected_layers {
        let indices: Vec<u32> = (0..layer_size).collect();
        let chosen = indices
            .into_iter()
            .choose_multiple(&mut rng, sample_points.min(layer_size as usize));
        point_indices.push(chosen);
    }

    Ok(KzgChallenge {
        vcr_id,
        layer_indices: selected_layers,
        point_indices,
        deadline_slot: 0,
    })
}
