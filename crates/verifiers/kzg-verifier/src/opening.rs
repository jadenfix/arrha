use aether_crypto_kzg::{KzgCommitment, KzgProof};
use aether_types::H256;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Opening {
    pub layer_idx: u32,
    pub point_idx: u32,
    /// Evaluation point (field element bytes).
    pub point: Vec<u8>,
    pub commitment: KzgCommitment,
    pub proof: KzgProof,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KzgOpeningResponse {
    pub vcr_id: H256,
    pub openings: Vec<Opening>,
}

impl KzgOpeningResponse {
    pub fn new(vcr_id: H256, openings: Vec<Opening>) -> Self {
        KzgOpeningResponse { vcr_id, openings }
    }
}
