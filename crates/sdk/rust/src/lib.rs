// ============================================================================
// AETHER RUST SDK - Client Library
// ============================================================================
// PURPOSE: Ergonomic Rust API for building Aether applications
//
// FEATURES:
//   - Transaction building
//   - Account management
//   - RPC client
//   - Contract calls
//   - AI job submission
//
// EXAMPLE:
// ```
// let client = AetherClient::new("http://localhost:8545");
// let keypair = Keypair::generate();
//
// // Transfer AIC
// let tx = client.transfer()
//     .to(recipient)
//     .amount(1000)
//     .token(TokenType::AIC)
//     .build()?;
//
// let result = client.submit(tx).await?;
// ```
// ============================================================================

pub mod agent_builder;
pub mod agent_identity;
pub mod chain;
pub mod client;
pub mod error;
pub mod job_builder;
pub mod payment;
pub mod transaction_builder;
pub mod types;

pub use aether_agent_schema::{
    journal_proof, journal_root_from_receipt_hashes, AgentAuthorization, AgentRunId, JournalProof,
    JournalProofNode, JournalRoot, MerkleSide, PaymentEnvelope, PaymentToken, RunStatus,
    SettlementPolicy, SideEffect, SignatureEnvelope, SigningAlgorithm, StepKind, StepReceipt,
    StepReceiptSigningPayload, AGENT_AUTHORIZATION_SIGNATURE_DOMAIN, PAYMENT_SIGNATURE_DOMAIN,
    STEP_RECEIPT_SIGNATURE_DOMAIN,
};
pub use aether_crypto_pq::{
    HybridIdentityProof, MlDsa87Keypair, PqSignatureAlgorithm, PqSignatureEnvelope,
};
pub use agent_builder::{
    AgentAuthorizationBuilder, PaymentEnvelopeBuilder, SignatureEnvelopeBuilder, StepReceiptBuilder,
};
pub use agent_identity::{
    agent_identity_message, generate_hybrid_agent_identity, sign_hybrid_agent_identity,
    verify_hybrid_agent_identity,
};
pub use client::AetherClient;
pub use error::AetherSdkError;
pub use job_builder::JobBuilder;
pub use payment::{
    build_payment_required_response, decode_payment_header, decode_payment_header_at,
    decode_payment_header_with_hash, encode_payment_header, payment_envelope_hash, payment_headers,
    validate_payment_envelope, PaymentAcceptExtra, PaymentAcceptOption, PaymentRequiredOptions,
    PaymentRequiredResponse, AETHER_PAYMENT_HASH_HEADER, AETHER_PAYMENT_HEADER,
    AETHER_PAYMENT_SCHEME,
};
pub use types::{NodeHealth, RpcAccount, RpcBlock, RpcReceipt};

#[cfg(test)]
mod proptest_tests;
