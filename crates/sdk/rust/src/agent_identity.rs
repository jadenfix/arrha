use aether_crypto_pq::{
    generate_ml_dsa87_keypair, pq_signature_context_for_alg, sign_ml_dsa87, verify_pq_signature,
    HybridIdentityProof, MlDsa87Keypair, PqCryptoError, PqSignatureAlgorithm,
};
use aether_crypto_primitives::{verify as verify_ed25519, Keypair};
use aether_types::{Address, H256};

use crate::error::AetherSdkError;

pub const AGENT_IDENTITY_DOMAIN: &str = "agent_identity";
pub const CLASSICAL_AGENT_IDENTITY_ALG: &str = "ed25519";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedHybridAgentIdentity {
    pub ed25519_public_key: Vec<u8>,
    pub ed25519_secret_key: Vec<u8>,
    pub ml_dsa87: MlDsa87Keypair,
}

pub fn generate_hybrid_agent_identity() -> Result<GeneratedHybridAgentIdentity, AetherSdkError> {
    let ed25519 = Keypair::generate();
    let ml_dsa87 = generate_ml_dsa87_keypair().map_err(map_pq_error)?;
    Ok(GeneratedHybridAgentIdentity {
        ed25519_public_key: ed25519.public_key(),
        ed25519_secret_key: ed25519.secret_key(),
        ml_dsa87,
    })
}

pub fn agent_identity_message(
    chain_id: u64,
    agent_account: Address,
    credential_root: H256,
) -> Vec<u8> {
    let mut message = Vec::with_capacity(
        b"aether/agent_identity/v1".len() + 1 + 8 + agent_account.as_bytes().len() + 32,
    );
    message.extend_from_slice(b"aether/agent_identity/v1");
    message.push(0);
    message.extend_from_slice(&chain_id.to_le_bytes());
    message.extend_from_slice(agent_account.as_bytes());
    message.extend_from_slice(credential_root.as_bytes());
    message
}

pub fn sign_hybrid_agent_identity(
    ed25519: &Keypair,
    ml_dsa87_private_key: &[u8],
    chain_id: u64,
    agent_account: Address,
    credential_root: H256,
) -> Result<HybridIdentityProof, AetherSdkError> {
    let message = agent_identity_message(chain_id, agent_account, credential_root);
    let pq_context = pq_signature_context_for_alg(
        AGENT_IDENTITY_DOMAIN,
        chain_id,
        PqSignatureAlgorithm::MlDsa87,
    );
    let pq_signature =
        sign_ml_dsa87(ml_dsa87_private_key, &pq_context, &message).map_err(map_pq_error)?;

    Ok(HybridIdentityProof {
        classical_alg: CLASSICAL_AGENT_IDENTITY_ALG.to_string(),
        classical_public_key: ed25519.public_key(),
        classical_signature: ed25519.sign(&message),
        pq_signature,
    })
}

#[must_use = "discarding hybrid identity verification results is a security bug"]
pub fn verify_hybrid_agent_identity(
    proof: &HybridIdentityProof,
    chain_id: u64,
    agent_account: Address,
    credential_root: H256,
) -> Result<(), AetherSdkError> {
    if proof.classical_alg != CLASSICAL_AGENT_IDENTITY_ALG {
        return Err(AetherSdkError::InvalidSignature(format!(
            "unsupported classical identity algorithm: {}",
            proof.classical_alg
        )));
    }

    if proof.pq_signature.alg != PqSignatureAlgorithm::MlDsa87 {
        return Err(AetherSdkError::InvalidSignature(
            "agent identity requires ML-DSA-87 post-quantum signature".to_string(),
        ));
    }

    let expected_context = pq_signature_context_for_alg(
        AGENT_IDENTITY_DOMAIN,
        chain_id,
        PqSignatureAlgorithm::MlDsa87,
    );
    if proof.pq_signature.context != expected_context {
        return Err(AetherSdkError::InvalidSignature(
            "agent identity post-quantum context mismatch".to_string(),
        ));
    }

    let message = agent_identity_message(chain_id, agent_account, credential_root);
    verify_ed25519(
        &proof.classical_public_key,
        &message,
        &proof.classical_signature,
    )
    .map_err(|err| AetherSdkError::InvalidSignature(err.to_string()))?;
    verify_pq_signature(&proof.pq_signature, &message).map_err(map_pq_error)?;
    Ok(())
}

fn map_pq_error(err: PqCryptoError) -> AetherSdkError {
    AetherSdkError::InvalidSignature(err.to_string())
}
