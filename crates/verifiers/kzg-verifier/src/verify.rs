use std::collections::HashSet;

use aether_crypto_kzg::KzgVerifier;

use crate::challenge::KzgChallenge;
use crate::error::{Result, VerifierError};
use crate::opening::{KzgOpeningResponse, Opening};

pub fn verify_kzg_openings(
    verifier: &KzgVerifier,
    challenge: &KzgChallenge,
    response: &KzgOpeningResponse,
) -> Result<()> {
    challenge.validate()?;

    if challenge.vcr_id != response.vcr_id {
        return Err(VerifierError::VcrMismatch {
            expected: challenge.vcr_id,
            received: response.vcr_id,
        });
    }

    let expected = challenge.expected_openings();
    if expected != response.openings.len() {
        return Err(VerifierError::IncompleteResponse {
            expected,
            received: response.openings.len(),
        });
    }

    let mut seen = HashSet::new();

    for opening in &response.openings {
        validate_opening(challenge, opening, &mut seen)?;
        let point: [u8; 32] = opening
            .point
            .as_slice()
            .try_into()
            .map_err(|_| VerifierError::InvalidChallenge("point must be 32 bytes"))?;
        let valid = verifier
            .verify(&opening.commitment, &opening.proof, &point)
            .map_err(|err| VerifierError::InvalidProof {
                layer: opening.layer_idx,
                point: opening.point_idx,
                source: err,
            })?;
        if !valid {
            return Err(VerifierError::InvalidProof {
                layer: opening.layer_idx,
                point: opening.point_idx,
                source: anyhow::anyhow!("KZG proof verification failed"),
            });
        }
    }

    Ok(())
}

fn validate_opening(
    challenge: &KzgChallenge,
    opening: &Opening,
    seen: &mut HashSet<(u32, u32)>,
) -> Result<()> {
    if !challenge.contains(opening.layer_idx, opening.point_idx) {
        return Err(VerifierError::UnexpectedOpening {
            layer: opening.layer_idx,
            point: opening.point_idx,
        });
    }

    if !seen.insert((opening.layer_idx, opening.point_idx)) {
        return Err(VerifierError::DuplicateOpening {
            layer: opening.layer_idx,
            point: opening.point_idx,
        });
    }

    if opening.point.len() != 32 {
        return Err(VerifierError::InvalidChallenge(
            "evaluation point must be 32 bytes",
        ));
    }

    Ok(())
}
