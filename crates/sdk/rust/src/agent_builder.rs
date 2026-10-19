use aether_agent_schema::{
    AgentAuthorization, AgentRunId, PaymentEnvelope, PaymentToken, SideEffect, SignatureEnvelope,
    SigningAlgorithm, StepKind, StepReceipt,
};
use aether_types::{Address, Slot, H256};

use crate::error::AetherSdkError;

pub struct SignatureEnvelopeBuilder {
    alg: SigningAlgorithm,
    domain: Option<String>,
    chain_id: Option<u64>,
    key_id: Option<String>,
    payload_hash: Option<H256>,
    signature: Option<Vec<u8>>,
    pq_signature: Option<Vec<u8>>,
}

impl SignatureEnvelopeBuilder {
    pub fn new() -> Self {
        Self {
            alg: SigningAlgorithm::Ed25519,
            domain: None,
            chain_id: None,
            key_id: None,
            payload_hash: None,
            signature: None,
            pq_signature: None,
        }
    }

    pub fn algorithm(mut self, alg: SigningAlgorithm) -> Self {
        self.alg = alg;
        self
    }

    pub fn domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    pub fn chain_id(mut self, chain_id: u64) -> Self {
        self.chain_id = Some(chain_id);
        self
    }

    pub fn key_id(mut self, key_id: impl Into<String>) -> Self {
        self.key_id = Some(key_id.into());
        self
    }

    pub fn payload_hash(mut self, payload_hash: H256) -> Self {
        self.payload_hash = Some(payload_hash);
        self
    }

    pub fn signature(mut self, signature: Vec<u8>) -> Self {
        self.signature = Some(signature);
        self
    }

    pub fn pq_signature(mut self, pq_signature: Vec<u8>) -> Self {
        self.pq_signature = Some(pq_signature);
        self
    }

    pub fn build(self) -> Result<SignatureEnvelope, AetherSdkError> {
        let envelope = SignatureEnvelope::new(
            self.alg,
            self.domain
                .ok_or_else(|| AetherSdkError::build("signature domain not set"))?,
            self.chain_id
                .ok_or_else(|| AetherSdkError::build("chain_id not set"))?,
            self.key_id
                .ok_or_else(|| AetherSdkError::build("key_id not set"))?,
            self.payload_hash
                .ok_or_else(|| AetherSdkError::build("payload_hash not set"))?,
            self.signature
                .ok_or_else(|| AetherSdkError::build("signature not set"))?,
            self.pq_signature,
        );
        envelope
            .validate()
            .map_err(|err| AetherSdkError::build(err.to_string()))?;
        Ok(envelope)
    }
}

impl Default for SignatureEnvelopeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

pub struct AgentAuthorizationBuilder {
    agent_account: Option<Address>,
    session_public_key: Option<Vec<u8>>,
    delegated_by: Option<Address>,
    valid_from_slot: Slot,
    valid_until_slot: Option<Slot>,
    max_aic: Option<u128>,
    max_per_call_aic: Option<u128>,
    allowed_side_effects: Vec<SideEffect>,
    allowed_tools: Vec<String>,
    allowed_recipients: Vec<Address>,
    policy_hash: Option<H256>,
    guardian_threshold: Option<u16>,
    guardian_public_key: Option<Vec<u8>>,
    signature: Option<SignatureEnvelope>,
}

impl AgentAuthorizationBuilder {
    pub fn new() -> Self {
        Self {
            agent_account: None,
            session_public_key: None,
            delegated_by: None,
            valid_from_slot: 0,
            valid_until_slot: None,
            max_aic: None,
            max_per_call_aic: None,
            allowed_side_effects: Vec::new(),
            allowed_tools: Vec::new(),
            allowed_recipients: Vec::new(),
            policy_hash: None,
            guardian_threshold: None,
            guardian_public_key: None,
            signature: None,
        }
    }

    pub fn agent_account(mut self, address: Address) -> Self {
        self.agent_account = Some(address);
        self
    }

    pub fn session_public_key(mut self, key: Vec<u8>) -> Self {
        self.session_public_key = Some(key);
        self
    }

    pub fn delegated_by(mut self, address: Address) -> Self {
        self.delegated_by = Some(address);
        self
    }

    pub fn valid_slots(mut self, from: Slot, until: Slot) -> Self {
        self.valid_from_slot = from;
        self.valid_until_slot = Some(until);
        self
    }

    pub fn spend_limits(mut self, max_aic: u128, max_per_call_aic: u128) -> Self {
        self.max_aic = Some(max_aic);
        self.max_per_call_aic = Some(max_per_call_aic);
        self
    }

    pub fn allow_side_effect(mut self, side_effect: SideEffect) -> Self {
        self.allowed_side_effects.push(side_effect);
        self
    }

    pub fn allow_tool(mut self, tool: impl Into<String>) -> Self {
        self.allowed_tools.push(tool.into());
        self
    }

    pub fn allow_recipient(mut self, recipient: Address) -> Self {
        self.allowed_recipients.push(recipient);
        self
    }

    pub fn policy_hash(mut self, hash: H256) -> Self {
        self.policy_hash = Some(hash);
        self
    }

    pub fn guardian_threshold(mut self, threshold: u16) -> Self {
        self.guardian_threshold = Some(threshold);
        self
    }

    pub fn guardian_public_key(mut self, public_key: Vec<u8>) -> Self {
        self.guardian_public_key = Some(public_key);
        self
    }

    pub fn signature(mut self, signature: SignatureEnvelope) -> Self {
        self.signature = Some(signature);
        self
    }

    pub fn build(self, current_slot: Slot) -> Result<AgentAuthorization, AetherSdkError> {
        let authorization = AgentAuthorization {
            agent_account: self
                .agent_account
                .ok_or_else(|| AetherSdkError::build("agent_account not set"))?,
            session_public_key: self
                .session_public_key
                .ok_or_else(|| AetherSdkError::build("session_public_key not set"))?,
            delegated_by: self
                .delegated_by
                .ok_or_else(|| AetherSdkError::build("delegated_by not set"))?,
            valid_from_slot: self.valid_from_slot,
            valid_until_slot: self
                .valid_until_slot
                .ok_or_else(|| AetherSdkError::build("valid_until_slot not set"))?,
            max_aic: self
                .max_aic
                .ok_or_else(|| AetherSdkError::build("max_aic not set"))?,
            max_per_call_aic: self
                .max_per_call_aic
                .ok_or_else(|| AetherSdkError::build("max_per_call_aic not set"))?,
            allowed_side_effects: self.allowed_side_effects,
            allowed_tools: self.allowed_tools,
            allowed_recipients: self.allowed_recipients,
            policy_hash: self
                .policy_hash
                .ok_or_else(|| AetherSdkError::build("policy_hash not set"))?,
            guardian_threshold: self.guardian_threshold,
            guardian_public_key: self.guardian_public_key,
            signature: self
                .signature
                .ok_or_else(|| AetherSdkError::build("authorization signature not set"))?,
        };
        authorization
            .validate(current_slot)
            .map_err(|err| AetherSdkError::build(err.to_string()))?;
        Ok(authorization)
    }
}

impl Default for AgentAuthorizationBuilder {
    fn default() -> Self {
        Self::new()
    }
}

pub struct StepReceiptBuilder {
    run_id: Option<AgentRunId>,
    seq: Option<u64>,
    prev_receipt_hash: Option<H256>,
    kind: Option<StepKind>,
    side_effect: Option<SideEffect>,
    request_hash: Option<H256>,
    result_hash: Option<H256>,
    evidence_uri_hash: Option<H256>,
    tool_use_id: Option<String>,
    signer: Option<Address>,
    signature: Option<SignatureEnvelope>,
}

impl StepReceiptBuilder {
    pub fn new() -> Self {
        Self {
            run_id: None,
            seq: None,
            prev_receipt_hash: None,
            kind: None,
            side_effect: None,
            request_hash: None,
            result_hash: None,
            evidence_uri_hash: None,
            tool_use_id: None,
            signer: None,
            signature: None,
        }
    }

    pub fn run_id(mut self, run_id: AgentRunId) -> Self {
        self.run_id = Some(run_id);
        self
    }

    pub fn seq(mut self, seq: u64) -> Self {
        self.seq = Some(seq);
        self
    }

    pub fn prev_receipt_hash(mut self, hash: H256) -> Self {
        self.prev_receipt_hash = Some(hash);
        self
    }

    pub fn kind(mut self, kind: StepKind) -> Self {
        self.kind = Some(kind);
        self
    }

    pub fn side_effect(mut self, side_effect: SideEffect) -> Self {
        self.side_effect = Some(side_effect);
        self
    }

    pub fn request_hash(mut self, hash: H256) -> Self {
        self.request_hash = Some(hash);
        self
    }

    pub fn result_hash(mut self, hash: H256) -> Self {
        self.result_hash = Some(hash);
        self
    }

    pub fn evidence_uri_hash(mut self, hash: H256) -> Self {
        self.evidence_uri_hash = Some(hash);
        self
    }

    pub fn tool_use_id(mut self, tool_use_id: impl Into<String>) -> Self {
        self.tool_use_id = Some(tool_use_id.into());
        self
    }

    pub fn tool_identity(self, identity: impl Into<String>) -> Self {
        self.tool_use_id(identity)
    }

    pub fn signer(mut self, signer: Address) -> Self {
        self.signer = Some(signer);
        self
    }

    pub fn signature(mut self, signature: SignatureEnvelope) -> Self {
        self.signature = Some(signature);
        self
    }

    pub fn build(self) -> Result<StepReceipt, AetherSdkError> {
        let receipt = StepReceipt {
            run_id: self
                .run_id
                .ok_or_else(|| AetherSdkError::build("run_id not set"))?,
            seq: self
                .seq
                .ok_or_else(|| AetherSdkError::build("seq not set"))?,
            prev_receipt_hash: self.prev_receipt_hash,
            kind: self
                .kind
                .ok_or_else(|| AetherSdkError::build("step kind not set"))?,
            side_effect: self
                .side_effect
                .ok_or_else(|| AetherSdkError::build("side_effect not set"))?,
            request_hash: self
                .request_hash
                .ok_or_else(|| AetherSdkError::build("request_hash not set"))?,
            result_hash: self
                .result_hash
                .ok_or_else(|| AetherSdkError::build("result_hash not set"))?,
            evidence_uri_hash: self.evidence_uri_hash,
            tool_use_id: self
                .tool_use_id
                .ok_or_else(|| AetherSdkError::build("tool_use_id not set"))?,
            signer: self
                .signer
                .ok_or_else(|| AetherSdkError::build("signer not set"))?,
            signature: self
                .signature
                .ok_or_else(|| AetherSdkError::build("receipt signature not set"))?,
        };
        receipt
            .validate()
            .map_err(|err| AetherSdkError::build(err.to_string()))?;
        Ok(receipt)
    }
}

impl Default for StepReceiptBuilder {
    fn default() -> Self {
        Self::new()
    }
}

pub struct PaymentEnvelopeBuilder {
    token: PaymentToken,
    amount: Option<u128>,
    recipient: Option<Address>,
    quote_hash: Option<H256>,
    request_hash: Option<H256>,
    result_hash: Option<H256>,
    nonce: Option<[u8; 32]>,
    expires_at_slot: Option<Slot>,
    chain_id: Option<u64>,
    side_effect: Option<SideEffect>,
    max_replays: u32,
    signature: Option<SignatureEnvelope>,
}

impl PaymentEnvelopeBuilder {
    pub fn new() -> Self {
        Self {
            token: PaymentToken::Aic,
            amount: None,
            recipient: None,
            quote_hash: None,
            request_hash: None,
            result_hash: None,
            nonce: None,
            expires_at_slot: None,
            chain_id: None,
            side_effect: None,
            max_replays: 1,
            signature: None,
        }
    }

    pub fn token(mut self, token: PaymentToken) -> Self {
        self.token = token;
        self
    }

    pub fn amount(mut self, amount: u128) -> Self {
        self.amount = Some(amount);
        self
    }

    pub fn recipient(mut self, recipient: Address) -> Self {
        self.recipient = Some(recipient);
        self
    }

    pub fn quote_hash(mut self, hash: H256) -> Self {
        self.quote_hash = Some(hash);
        self
    }

    pub fn request_hash(mut self, hash: H256) -> Self {
        self.request_hash = Some(hash);
        self
    }

    pub fn result_hash(mut self, hash: H256) -> Self {
        self.result_hash = Some(hash);
        self
    }

    pub fn nonce(mut self, nonce: [u8; 32]) -> Self {
        self.nonce = Some(nonce);
        self
    }

    pub fn expires_at_slot(mut self, slot: Slot) -> Self {
        self.expires_at_slot = Some(slot);
        self
    }

    pub fn chain_id(mut self, chain_id: u64) -> Self {
        self.chain_id = Some(chain_id);
        self
    }

    pub fn side_effect(mut self, side_effect: SideEffect) -> Self {
        self.side_effect = Some(side_effect);
        self
    }

    pub fn max_replays(mut self, max_replays: u32) -> Self {
        self.max_replays = max_replays;
        self
    }

    pub fn signature(mut self, signature: SignatureEnvelope) -> Self {
        self.signature = Some(signature);
        self
    }

    pub fn signing_payload_hash(&self) -> Result<H256, AetherSdkError> {
        let envelope = PaymentEnvelope {
            token: self.token,
            amount: self
                .amount
                .ok_or_else(|| AetherSdkError::build("payment amount not set"))?,
            recipient: self
                .recipient
                .ok_or_else(|| AetherSdkError::build("payment recipient not set"))?,
            quote_hash: self
                .quote_hash
                .ok_or_else(|| AetherSdkError::build("quote_hash not set"))?,
            request_hash: self
                .request_hash
                .ok_or_else(|| AetherSdkError::build("request_hash not set"))?,
            result_hash: self.result_hash,
            nonce: self
                .nonce
                .ok_or_else(|| AetherSdkError::build("nonce not set"))?,
            expires_at_slot: self
                .expires_at_slot
                .ok_or_else(|| AetherSdkError::build("expires_at_slot not set"))?,
            chain_id: self
                .chain_id
                .ok_or_else(|| AetherSdkError::build("chain_id not set"))?,
            side_effect: self
                .side_effect
                .ok_or_else(|| AetherSdkError::build("payment side_effect not set"))?,
            max_replays: self.max_replays,
            signature: SignatureEnvelope::new(
                SigningAlgorithm::Ed25519,
                aether_agent_schema::PAYMENT_SIGNATURE_DOMAIN,
                self.chain_id
                    .ok_or_else(|| AetherSdkError::build("chain_id not set"))?,
                "unsigned-payment",
                H256::zero(),
                vec![1],
                None,
            ),
        };
        envelope
            .validate(0)
            .map_err(|err| AetherSdkError::build(err.to_string()))?;
        envelope
            .signing_payload_hash()
            .map_err(|err| AetherSdkError::build(err.to_string()))
    }

    pub fn build(self, current_slot: Slot) -> Result<PaymentEnvelope, AetherSdkError> {
        let envelope = PaymentEnvelope {
            token: self.token,
            amount: self
                .amount
                .ok_or_else(|| AetherSdkError::build("payment amount not set"))?,
            recipient: self
                .recipient
                .ok_or_else(|| AetherSdkError::build("payment recipient not set"))?,
            quote_hash: self
                .quote_hash
                .ok_or_else(|| AetherSdkError::build("quote_hash not set"))?,
            request_hash: self
                .request_hash
                .ok_or_else(|| AetherSdkError::build("request_hash not set"))?,
            result_hash: self.result_hash,
            nonce: self
                .nonce
                .ok_or_else(|| AetherSdkError::build("nonce not set"))?,
            expires_at_slot: self
                .expires_at_slot
                .ok_or_else(|| AetherSdkError::build("expires_at_slot not set"))?,
            chain_id: self
                .chain_id
                .ok_or_else(|| AetherSdkError::build("chain_id not set"))?,
            side_effect: self
                .side_effect
                .ok_or_else(|| AetherSdkError::build("payment side_effect not set"))?,
            max_replays: self.max_replays,
            signature: self
                .signature
                .ok_or_else(|| AetherSdkError::build("payment signature not set"))?,
        };
        envelope
            .validate(current_slot)
            .map_err(|err| AetherSdkError::build(err.to_string()))?;
        Ok(envelope)
    }
}

impl Default for PaymentEnvelopeBuilder {
    fn default() -> Self {
        Self::new()
    }
}
