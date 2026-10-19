//! Shared agent settlement schema.
//!
//! This crate intentionally contains wire-level data types and validation only.
//! Concrete signing, FROST, post-quantum, TEE, and zk verification live in
//! crypto/verifier crates that consume these envelopes.

#![recursion_limit = "256"]

use aether_types::{Address, Slot, H256};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

pub mod schema;
pub use schema::agent_contract_schema;

const DOMAIN_PREFIX: &str = "aether/";
pub const AGENT_AUTHORIZATION_SIGNATURE_DOMAIN: &str = "aether/agent_authorization/v1";
pub const PAYMENT_SIGNATURE_DOMAIN: &str = "aether/agent_payment_authorization/v1";
pub const STEP_RECEIPT_SIGNATURE_DOMAIN: &str = "aether/agent_step_receipt/v1";
pub const STEP_RECEIPT_ENVELOPE_DOMAIN: &str = "aether/agent_step_receipt_envelope/v1";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AgentSchemaError {
    #[error("domain must start with aether/")]
    InvalidDomain,
    #[error("chain_id must be non-zero")]
    InvalidChainId,
    #[error("key_id must not be empty")]
    EmptyKeyId,
    #[error("signature must not be empty")]
    EmptySignature,
    #[error("signature domain must be {expected}")]
    InvalidSignatureDomain { expected: &'static str },
    #[error("signature chain_id does not match the signed object")]
    SignatureChainIdMismatch,
    #[error("signature payload_hash does not match the canonical signing payload")]
    SignaturePayloadHashMismatch,
    #[error("post-quantum signature is required for {0:?}")]
    MissingPostQuantumSignature(SigningAlgorithm),
    #[error("expiration slot must be greater than the current slot")]
    Expired,
    #[error("spend limits are inconsistent")]
    InvalidSpendLimit,
    #[error("guardian threshold is required for high-risk side effects")]
    MissingGuardianThreshold,
    #[error("guardian public key is required for high-risk side effects")]
    MissingGuardianPublicKey,
    #[error("high-risk side effects require a FROST guardian signature")]
    MissingFrostGuardianSignature,
    #[error("allowed side effects must not be empty")]
    EmptySideEffects,
    #[error("payment amount must be non-zero")]
    ZeroPaymentAmount,
    #[error("payment recipient must not be zero")]
    ZeroPaymentRecipient,
    #[error("replay count must be non-zero")]
    ZeroReplayCount,
    #[error("request/result binding is required for settled payments")]
    MissingResultBinding,
    #[error("receipt sequence cannot be zero")]
    InvalidReceiptSequence,
    #[error("tool identity must not be empty")]
    EmptyToolIdentity,
    #[error("journal must contain at least one receipt hash")]
    EmptyJournal,
    #[error("journal leaf index is out of bounds")]
    InvalidJournalLeafIndex,
    #[error("journal proof does not match the expected root")]
    InvalidJournalProof,
    #[error("serialization failed: {0}")]
    Serialization(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SigningAlgorithm {
    Ed25519,
    Bls12381,
    FrostRistretto255,
    Ed25519MlDsa87,
    MlDsa87,
    SlhDsaSha2256f,
}

impl SigningAlgorithm {
    #[must_use]
    pub const fn requires_post_quantum_component(self) -> bool {
        matches!(
            self,
            SigningAlgorithm::Ed25519MlDsa87
                | SigningAlgorithm::MlDsa87
                | SigningAlgorithm::SlhDsaSha2256f
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureEnvelope {
    pub alg: SigningAlgorithm,
    pub domain: String,
    pub chain_id: u64,
    pub key_id: String,
    #[serde(with = "wire::h256")]
    pub payload_hash: H256,
    #[serde(with = "wire::bytes")]
    pub signature: Vec<u8>,
    #[serde(default, with = "wire::option_bytes")]
    pub pq_signature: Option<Vec<u8>>,
}

impl SignatureEnvelope {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        alg: SigningAlgorithm,
        domain: impl Into<String>,
        chain_id: u64,
        key_id: impl Into<String>,
        payload_hash: H256,
        signature: Vec<u8>,
        pq_signature: Option<Vec<u8>>,
    ) -> Self {
        Self {
            alg,
            domain: domain.into(),
            chain_id,
            key_id: key_id.into(),
            payload_hash,
            signature,
            pq_signature,
        }
    }

    pub fn validate(&self) -> Result<(), AgentSchemaError> {
        validate_domain(&self.domain)?;
        validate_chain_id(self.chain_id)?;
        if self.key_id.trim().is_empty() {
            return Err(AgentSchemaError::EmptyKeyId);
        }
        if self.signature.is_empty() {
            return Err(AgentSchemaError::EmptySignature);
        }
        if self.alg.requires_post_quantum_component()
            && self.pq_signature.as_ref().is_none_or(Vec::is_empty)
        {
            return Err(AgentSchemaError::MissingPostQuantumSignature(self.alg));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffect {
    Read,
    Draft,
    Write,
    Send,
    Purchase,
    Delete,
}

impl SideEffect {
    #[must_use]
    pub const fn requires_guardian(self) -> bool {
        matches!(
            self,
            SideEffect::Send | SideEffect::Purchase | SideEffect::Delete
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    LlmCall,
    ToolCall,
    BrowseAct,
    SandboxExec,
    McpCall,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Completed,
    Failed,
    NeedsReview,
    Disputed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct AgentRunId(pub [u8; 32]);

impl AgentRunId {
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Serialize for AgentRunId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if serializer.is_human_readable() {
            serializer.serialize_str(&wire::encode_hex(&self.0))
        } else {
            self.0.serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for AgentRunId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            let encoded = String::deserialize(deserializer)?;
            let bytes = wire::decode_fixed_hex::<32>(&encoded).map_err(serde::de::Error::custom)?;
            Ok(Self(bytes))
        } else {
            <[u8; 32]>::deserialize(deserializer).map(Self)
        }
    }
}

/// Count-bound Merkle commitment over ordered step receipt hashes.
///
/// The public root is not the raw padded binary Merkle root. It is
/// `H("aether/agent_journal_root/v1" || 0x00 || leaf_count_le || raw_root)`,
/// so an odd-length journal cannot share a root with its padded next length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct JournalRoot(pub H256);

impl Default for JournalRoot {
    fn default() -> Self {
        Self(H256::zero())
    }
}

impl JournalRoot {
    #[must_use]
    pub const fn new(hash: H256) -> Self {
        Self(hash)
    }

    #[must_use]
    pub const fn as_hash(&self) -> H256 {
        self.0
    }
}

impl Serialize for JournalRoot {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if serializer.is_human_readable() {
            serializer.serialize_str(&wire::encode_hex(self.0.as_bytes()))
        } else {
            self.0.serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for JournalRoot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            let encoded = String::deserialize(deserializer)?;
            let bytes = wire::decode_fixed_hex::<32>(&encoded).map_err(serde::de::Error::custom)?;
            Ok(Self(H256::from(bytes)))
        } else {
            H256::deserialize(deserializer).map(Self)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MerkleSide {
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalProofNode {
    pub side: MerkleSide,
    #[serde(with = "wire::h256")]
    pub hash: H256,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalProof {
    #[serde(with = "wire::h256")]
    pub leaf_hash: H256,
    pub leaf_index: u64,
    pub leaf_count: u64,
    pub siblings: Vec<JournalProofNode>,
}

impl JournalProof {
    pub fn verify(&self, root: JournalRoot) -> Result<(), AgentSchemaError> {
        if self.leaf_count == 0 || self.leaf_index >= self.leaf_count {
            return Err(AgentSchemaError::InvalidJournalLeafIndex);
        }

        let mut hash = self.leaf_hash;
        let mut index = self.leaf_index;
        let mut width = self.leaf_count;
        let mut siblings = self.siblings.iter();

        while width > 1 {
            let sibling = siblings
                .next()
                .ok_or(AgentSchemaError::InvalidJournalProof)?;
            let expected_side = if index % 2 == 0 {
                MerkleSide::Right
            } else {
                MerkleSide::Left
            };
            if sibling.side != expected_side {
                return Err(AgentSchemaError::InvalidJournalProof);
            }

            hash = match expected_side {
                MerkleSide::Left => journal_node_hash(sibling.hash, hash),
                MerkleSide::Right => journal_node_hash(hash, sibling.hash),
            };
            index /= 2;
            width = width.div_ceil(2);
        }

        if siblings.next().is_some() {
            return Err(AgentSchemaError::InvalidJournalProof);
        }

        if journal_root_commitment(self.leaf_count, hash) == root.0 {
            Ok(())
        } else {
            Err(AgentSchemaError::InvalidJournalProof)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementPolicy {
    pub min_escrow_aic: u128,
    pub challenge_slots: u64,
    #[serde(default)]
    pub requires_human_confirm: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAuthorization {
    #[serde(with = "wire::address")]
    pub agent_account: Address,
    #[serde(with = "wire::bytes")]
    pub session_public_key: Vec<u8>,
    #[serde(with = "wire::address")]
    pub delegated_by: Address,
    pub valid_from_slot: Slot,
    pub valid_until_slot: Slot,
    pub max_aic: u128,
    pub max_per_call_aic: u128,
    pub allowed_side_effects: Vec<SideEffect>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default, with = "wire::address_vec")]
    pub allowed_recipients: Vec<Address>,
    #[serde(with = "wire::h256")]
    pub policy_hash: H256,
    #[serde(default)]
    pub guardian_threshold: Option<u16>,
    #[serde(default, with = "wire::option_bytes")]
    pub guardian_public_key: Option<Vec<u8>>,
    pub signature: SignatureEnvelope,
}

impl AgentAuthorization {
    pub fn validate(&self, current_slot: Slot) -> Result<(), AgentSchemaError> {
        self.signature.validate()?;
        if self.signature.domain != AGENT_AUTHORIZATION_SIGNATURE_DOMAIN {
            return Err(AgentSchemaError::InvalidSignatureDomain {
                expected: AGENT_AUTHORIZATION_SIGNATURE_DOMAIN,
            });
        }
        if self.valid_until_slot <= current_slot || self.valid_until_slot <= self.valid_from_slot {
            return Err(AgentSchemaError::Expired);
        }
        if self.max_per_call_aic > self.max_aic {
            return Err(AgentSchemaError::InvalidSpendLimit);
        }
        if self.allowed_side_effects.is_empty() {
            return Err(AgentSchemaError::EmptySideEffects);
        }
        if self
            .allowed_side_effects
            .iter()
            .any(|effect| effect.requires_guardian())
        {
            if self.guardian_threshold.unwrap_or_default() == 0 {
                return Err(AgentSchemaError::MissingGuardianThreshold);
            }
            if self.guardian_public_key.as_ref().is_none_or(Vec::is_empty) {
                return Err(AgentSchemaError::MissingGuardianPublicKey);
            }
            if self.signature.alg != SigningAlgorithm::FrostRistretto255 {
                return Err(AgentSchemaError::MissingFrostGuardianSignature);
            }
        }
        Ok(())
    }

    /// Validate the object-specific guardian signature envelope binding.
    ///
    /// This does not verify the FROST signature bytes. Runtime callers must
    /// still verify `signature.signature` against `guardian_public_key` over
    /// the same `expected_payload_hash`. This helper guarantees the envelope
    /// declares the canonical agent-authorization domain and commits to the
    /// operation policy hash that the runtime is about to verify.
    pub fn validate_guardian_signature_binding(
        &self,
        expected_payload_hash: H256,
    ) -> Result<(), AgentSchemaError> {
        self.signature.validate()?;
        if self.signature.domain != AGENT_AUTHORIZATION_SIGNATURE_DOMAIN {
            return Err(AgentSchemaError::InvalidSignatureDomain {
                expected: AGENT_AUTHORIZATION_SIGNATURE_DOMAIN,
            });
        }
        if self.signature.payload_hash != expected_payload_hash {
            return Err(AgentSchemaError::SignaturePayloadHashMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepReceipt {
    pub run_id: AgentRunId,
    pub seq: u64,
    #[serde(default, with = "wire::option_h256")]
    pub prev_receipt_hash: Option<H256>,
    pub kind: StepKind,
    pub side_effect: SideEffect,
    #[serde(with = "wire::h256")]
    pub request_hash: H256,
    #[serde(with = "wire::h256")]
    pub result_hash: H256,
    #[serde(default, with = "wire::option_h256")]
    pub evidence_uri_hash: Option<H256>,
    #[serde(alias = "tool_identity")]
    pub tool_use_id: String,
    #[serde(with = "wire::address")]
    pub signer: Address,
    pub signature: SignatureEnvelope,
}

impl StepReceipt {
    pub fn validate(&self) -> Result<(), AgentSchemaError> {
        self.signature.validate()?;
        if self.signature.domain != STEP_RECEIPT_SIGNATURE_DOMAIN {
            return Err(AgentSchemaError::InvalidSignatureDomain {
                expected: STEP_RECEIPT_SIGNATURE_DOMAIN,
            });
        }
        if self.seq == 0 {
            return Err(AgentSchemaError::InvalidReceiptSequence);
        }
        if self.tool_use_id.trim().is_empty() {
            return Err(AgentSchemaError::EmptyToolIdentity);
        }
        if self.signature.payload_hash != self.signing_payload_hash()? {
            return Err(AgentSchemaError::SignaturePayloadHashMismatch);
        }
        Ok(())
    }

    #[must_use]
    pub fn signing_payload(&self) -> StepReceiptSigningPayload {
        StepReceiptSigningPayload::from(self)
    }

    pub fn signing_payload_hash(&self) -> Result<H256, AgentSchemaError> {
        self.signing_payload().signing_payload_hash()
    }

    pub fn receipt_hash(&self) -> Result<H256, AgentSchemaError> {
        typed_hash(STEP_RECEIPT_ENVELOPE_DOMAIN, self)
    }
}

/// Canonical receipt payload signed by an agent runtime.
///
/// This payload deliberately excludes the `SignatureEnvelope` so a receipt's
/// signer commits to the run, step sequence, side-effect class, request/result
/// hashes, tool-use id, and signer address without recursively signing its own
/// signature bytes. `StepReceipt::validate` requires the receipt signature
/// domain to be [`STEP_RECEIPT_SIGNATURE_DOMAIN`] and the envelope
/// `payload_hash` to match this payload exactly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepReceiptSigningPayload {
    pub run_id: AgentRunId,
    pub seq: u64,
    #[serde(default, with = "wire::option_h256")]
    pub prev_receipt_hash: Option<H256>,
    pub kind: StepKind,
    pub side_effect: SideEffect,
    #[serde(with = "wire::h256")]
    pub request_hash: H256,
    #[serde(with = "wire::h256")]
    pub result_hash: H256,
    #[serde(default, with = "wire::option_h256")]
    pub evidence_uri_hash: Option<H256>,
    #[serde(alias = "tool_identity")]
    pub tool_use_id: String,
    #[serde(with = "wire::address")]
    pub signer: Address,
}

impl StepReceiptSigningPayload {
    pub fn signing_payload_hash(&self) -> Result<H256, AgentSchemaError> {
        typed_hash(STEP_RECEIPT_SIGNATURE_DOMAIN, self)
    }
}

impl From<&StepReceipt> for StepReceiptSigningPayload {
    fn from(receipt: &StepReceipt) -> Self {
        Self {
            run_id: receipt.run_id,
            seq: receipt.seq,
            prev_receipt_hash: receipt.prev_receipt_hash,
            kind: receipt.kind,
            side_effect: receipt.side_effect,
            request_hash: receipt.request_hash,
            result_hash: receipt.result_hash,
            evidence_uri_hash: receipt.evidence_uri_hash,
            tool_use_id: receipt.tool_use_id.clone(),
            signer: receipt.signer,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentToken {
    Aic,
    Swr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentEnvelope {
    pub token: PaymentToken,
    #[serde(with = "wire::u128_decimal_string")]
    pub amount: u128,
    #[serde(with = "wire::address")]
    pub recipient: Address,
    #[serde(with = "wire::h256")]
    pub quote_hash: H256,
    #[serde(with = "wire::h256")]
    pub request_hash: H256,
    #[serde(default, with = "wire::option_h256")]
    pub result_hash: Option<H256>,
    #[serde(with = "wire::bytes32")]
    pub nonce: [u8; 32],
    pub expires_at_slot: Slot,
    pub chain_id: u64,
    pub side_effect: SideEffect,
    #[serde(default = "default_max_replays")]
    pub max_replays: u32,
    pub signature: SignatureEnvelope,
}

impl PaymentEnvelope {
    pub fn validate(&self, current_slot: Slot) -> Result<(), AgentSchemaError> {
        self.signature.validate()?;
        validate_chain_id(self.chain_id)?;
        if self.amount == 0 {
            return Err(AgentSchemaError::ZeroPaymentAmount);
        }
        if self.recipient == Address::from([0u8; 20]) {
            return Err(AgentSchemaError::ZeroPaymentRecipient);
        }
        if self.expires_at_slot <= current_slot {
            return Err(AgentSchemaError::Expired);
        }
        if self.max_replays == 0 {
            return Err(AgentSchemaError::ZeroReplayCount);
        }
        if self.side_effect.requires_guardian() && self.result_hash.is_none() {
            return Err(AgentSchemaError::MissingResultBinding);
        }
        Ok(())
    }

    /// Validate the object-specific payment signature envelope binding.
    ///
    /// This does not verify the Ed25519 signature bytes. Runtime callers must
    /// still verify `signature.signature` against the authorized session key
    /// over the returned canonical signing payload hash.
    pub fn validate_signature_binding(&self) -> Result<H256, AgentSchemaError> {
        self.signature.validate()?;
        validate_chain_id(self.chain_id)?;
        if self.signature.domain != PAYMENT_SIGNATURE_DOMAIN {
            return Err(AgentSchemaError::InvalidSignatureDomain {
                expected: PAYMENT_SIGNATURE_DOMAIN,
            });
        }
        if self.signature.chain_id != self.chain_id {
            return Err(AgentSchemaError::SignatureChainIdMismatch);
        }
        let payload_hash = self.signing_payload_hash()?;
        if self.signature.payload_hash != payload_hash {
            return Err(AgentSchemaError::SignaturePayloadHashMismatch);
        }
        Ok(payload_hash)
    }

    pub fn payment_hash(&self) -> Result<H256, AgentSchemaError> {
        typed_hash("aether/agent_payment_envelope/v1", self)
    }

    pub fn signing_payload_hash(&self) -> Result<H256, AgentSchemaError> {
        typed_hash(
            PAYMENT_SIGNATURE_DOMAIN,
            &PaymentEnvelopeSigningPayload {
                token: self.token,
                amount: self.amount,
                recipient: self.recipient,
                quote_hash: self.quote_hash,
                request_hash: self.request_hash,
                result_hash: self.result_hash,
                nonce: self.nonce,
                expires_at_slot: self.expires_at_slot,
                chain_id: self.chain_id,
                side_effect: self.side_effect,
                max_replays: self.max_replays,
            },
        )
    }
}

fn default_max_replays() -> u32 {
    1
}

#[derive(Serialize)]
struct PaymentEnvelopeSigningPayload {
    token: PaymentToken,
    amount: u128,
    recipient: Address,
    quote_hash: H256,
    request_hash: H256,
    result_hash: Option<H256>,
    nonce: [u8; 32],
    expires_at_slot: Slot,
    chain_id: u64,
    side_effect: SideEffect,
    max_replays: u32,
}

pub fn typed_hash<T: Serialize>(domain: &str, value: &T) -> Result<H256, AgentSchemaError> {
    validate_domain(domain)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    hasher.update(&[0]);
    let encoded = bincode::serialize(value)
        .map_err(|err| AgentSchemaError::Serialization(err.to_string()))?;
    hasher.update(&encoded);
    let mut out = [0u8; 32];
    out.copy_from_slice(hasher.finalize().as_bytes());
    Ok(H256::from(out))
}

pub fn journal_root_from_receipt_hashes(
    receipt_hashes: &[H256],
) -> Result<JournalRoot, AgentSchemaError> {
    if receipt_hashes.is_empty() {
        return Err(AgentSchemaError::EmptyJournal);
    }
    Ok(JournalRoot(journal_root_commitment(
        receipt_hashes.len() as u64,
        raw_journal_merkle_root(receipt_hashes),
    )))
}

pub fn journal_proof(
    receipt_hashes: &[H256],
    leaf_index: usize,
) -> Result<JournalProof, AgentSchemaError> {
    if receipt_hashes.is_empty() {
        return Err(AgentSchemaError::EmptyJournal);
    }
    if leaf_index >= receipt_hashes.len() {
        return Err(AgentSchemaError::InvalidJournalLeafIndex);
    }

    let mut level = receipt_hashes.to_vec();
    let mut index = leaf_index;
    let mut siblings = Vec::new();

    while level.len() > 1 {
        let sibling_index = if index % 2 == 0 {
            (index + 1).min(level.len() - 1)
        } else {
            index - 1
        };
        siblings.push(JournalProofNode {
            side: if index % 2 == 0 {
                MerkleSide::Right
            } else {
                MerkleSide::Left
            },
            hash: level[sibling_index],
        });

        level = journal_next_level(&level);
        index /= 2;
    }

    Ok(JournalProof {
        leaf_hash: receipt_hashes[leaf_index],
        leaf_index: leaf_index as u64,
        leaf_count: receipt_hashes.len() as u64,
        siblings,
    })
}

fn raw_journal_merkle_root(receipt_hashes: &[H256]) -> H256 {
    let mut level = receipt_hashes.to_vec();
    while level.len() > 1 {
        level = journal_next_level(&level);
    }
    level[0]
}

fn journal_root_commitment(leaf_count: u64, raw_merkle_root: H256) -> H256 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"aether/agent_journal_root/v1");
    hasher.update(&[0]);
    hasher.update(&leaf_count.to_le_bytes());
    hasher.update(raw_merkle_root.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(hasher.finalize().as_bytes());
    H256::from(out)
}

fn journal_next_level(level: &[H256]) -> Vec<H256> {
    level
        .chunks(2)
        .map(|pair| {
            let left = pair[0];
            let right = pair.get(1).copied().unwrap_or(left);
            journal_node_hash(left, right)
        })
        .collect()
}

fn journal_node_hash(left: H256, right: H256) -> H256 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"aether/agent_journal_node/v1");
    hasher.update(&[0]);
    hasher.update(left.as_bytes());
    hasher.update(right.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(hasher.finalize().as_bytes());
    H256::from(out)
}

fn validate_domain(domain: &str) -> Result<(), AgentSchemaError> {
    if !domain.starts_with(DOMAIN_PREFIX) {
        return Err(AgentSchemaError::InvalidDomain);
    }
    Ok(())
}

fn validate_chain_id(chain_id: u64) -> Result<(), AgentSchemaError> {
    if chain_id == 0 {
        return Err(AgentSchemaError::InvalidChainId);
    }
    Ok(())
}

mod wire {
    use super::*;

    pub(super) fn encode_hex(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(2 + bytes.len() * 2);
        encoded.push_str("0x");
        for byte in bytes {
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
        encoded
    }

    pub(super) fn decode_fixed_hex<const N: usize>(encoded: &str) -> Result<[u8; N], String> {
        let bytes = decode_hex(encoded)?;
        if bytes.len() != N {
            return Err(format!("expected {N} bytes, got {}", bytes.len()));
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&bytes);
        Ok(out)
    }

    fn decode_hex(encoded: &str) -> Result<Vec<u8>, String> {
        let body = encoded
            .strip_prefix("0x")
            .ok_or_else(|| "hex value must start with 0x".to_string())?;
        if body.len() % 2 != 0 {
            return Err("hex value must contain an even number of digits".to_string());
        }

        let mut out = Vec::with_capacity(body.len() / 2);
        for pair in body.as_bytes().chunks(2) {
            let high = decode_nibble(pair[0])
                .ok_or_else(|| format!("invalid hex digit '{}'", pair[0] as char))?;
            let low = decode_nibble(pair[1])
                .ok_or_else(|| format!("invalid hex digit '{}'", pair[1] as char))?;
            out.push((high << 4) | low);
        }
        Ok(out)
    }

    fn decode_nibble(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    pub(super) mod h256 {
        use super::*;

        pub fn serialize<S>(value: &H256, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            if serializer.is_human_readable() {
                serializer.serialize_str(&encode_hex(value.as_bytes()))
            } else {
                value.serialize(serializer)
            }
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<H256, D::Error>
        where
            D: Deserializer<'de>,
        {
            if deserializer.is_human_readable() {
                let encoded = String::deserialize(deserializer)?;
                let bytes = decode_fixed_hex::<32>(&encoded).map_err(serde::de::Error::custom)?;
                Ok(H256::from(bytes))
            } else {
                H256::deserialize(deserializer)
            }
        }
    }

    pub(super) mod option_h256 {
        use super::*;

        pub fn serialize<S>(value: &Option<H256>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            if serializer.is_human_readable() {
                match value {
                    Some(hash) => serializer.serialize_some(&encode_hex(hash.as_bytes())),
                    None => serializer.serialize_none(),
                }
            } else {
                value.serialize(serializer)
            }
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<H256>, D::Error>
        where
            D: Deserializer<'de>,
        {
            if deserializer.is_human_readable() {
                let encoded = Option::<String>::deserialize(deserializer)?;
                encoded
                    .map(|value| {
                        decode_fixed_hex::<32>(&value)
                            .map(H256::from)
                            .map_err(serde::de::Error::custom)
                    })
                    .transpose()
            } else {
                Option::<H256>::deserialize(deserializer)
            }
        }
    }

    pub(super) mod address {
        use super::*;

        pub fn serialize<S>(value: &Address, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            if serializer.is_human_readable() {
                serializer.serialize_str(&encode_hex(value.as_bytes()))
            } else {
                value.serialize(serializer)
            }
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Address, D::Error>
        where
            D: Deserializer<'de>,
        {
            if deserializer.is_human_readable() {
                let encoded = String::deserialize(deserializer)?;
                let bytes = decode_fixed_hex::<20>(&encoded).map_err(serde::de::Error::custom)?;
                Ok(Address::from(bytes))
            } else {
                Address::deserialize(deserializer)
            }
        }
    }

    pub(super) mod address_vec {
        use super::*;

        pub fn serialize<S>(value: &Vec<Address>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            if serializer.is_human_readable() {
                let encoded: Vec<String> = value
                    .iter()
                    .map(|address| encode_hex(address.as_bytes()))
                    .collect();
                encoded.serialize(serializer)
            } else {
                value.serialize(serializer)
            }
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<Address>, D::Error>
        where
            D: Deserializer<'de>,
        {
            if deserializer.is_human_readable() {
                let encoded = Vec::<String>::deserialize(deserializer)?;
                encoded
                    .into_iter()
                    .map(|value| {
                        decode_fixed_hex::<20>(&value)
                            .map(Address::from)
                            .map_err(serde::de::Error::custom)
                    })
                    .collect()
            } else {
                Vec::<Address>::deserialize(deserializer)
            }
        }
    }

    pub(super) mod bytes {
        use super::*;

        pub fn serialize<S>(value: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            if serializer.is_human_readable() {
                serializer.serialize_str(&encode_hex(value))
            } else {
                value.serialize(serializer)
            }
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
        where
            D: Deserializer<'de>,
        {
            if deserializer.is_human_readable() {
                let encoded = String::deserialize(deserializer)?;
                decode_hex(&encoded).map_err(serde::de::Error::custom)
            } else {
                Vec::<u8>::deserialize(deserializer)
            }
        }
    }

    pub(super) mod option_bytes {
        use super::*;

        pub fn serialize<S>(value: &Option<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            if serializer.is_human_readable() {
                match value {
                    Some(bytes) => serializer.serialize_some(&encode_hex(bytes)),
                    None => serializer.serialize_none(),
                }
            } else {
                value.serialize(serializer)
            }
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error>
        where
            D: Deserializer<'de>,
        {
            if deserializer.is_human_readable() {
                let encoded = Option::<String>::deserialize(deserializer)?;
                encoded
                    .map(|value| decode_hex(&value).map_err(serde::de::Error::custom))
                    .transpose()
            } else {
                Option::<Vec<u8>>::deserialize(deserializer)
            }
        }
    }

    pub(super) mod bytes32 {
        use super::*;

        pub fn serialize<S>(value: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            if serializer.is_human_readable() {
                serializer.serialize_str(&encode_hex(value))
            } else {
                value.serialize(serializer)
            }
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 32], D::Error>
        where
            D: Deserializer<'de>,
        {
            if deserializer.is_human_readable() {
                let encoded = String::deserialize(deserializer)?;
                decode_fixed_hex::<32>(&encoded).map_err(serde::de::Error::custom)
            } else {
                <[u8; 32]>::deserialize(deserializer)
            }
        }
    }

    pub(super) mod u128_decimal_string {
        use super::*;

        pub fn serialize<S>(value: &u128, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            if serializer.is_human_readable() {
                serializer.serialize_str(&value.to_string())
            } else {
                value.serialize(serializer)
            }
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<u128, D::Error>
        where
            D: Deserializer<'de>,
        {
            if deserializer.is_human_readable() {
                let encoded = String::deserialize(deserializer)?;
                parse_canonical_u128(&encoded).map_err(serde::de::Error::custom)
            } else {
                u128::deserialize(deserializer)
            }
        }

        fn parse_canonical_u128(encoded: &str) -> Result<u128, String> {
            if encoded.is_empty() {
                return Err("amount must not be empty".to_string());
            }
            if !encoded.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err("amount must be a base-10 integer string".to_string());
            }
            if encoded.len() > 1 && encoded.starts_with('0') {
                return Err("amount must use canonical decimal form".to_string());
            }
            encoded
                .parse::<u128>()
                .map_err(|err| format!("amount is outside u128 range: {err}"))
        }
    }
}
