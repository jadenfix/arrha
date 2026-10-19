//! Account Abstraction (ERC-4337 style) for Aether.
//!
//! Enables programmable accounts that support:
//! - Custom signature validation (multisig, social recovery)
//! - Gas sponsorship (paymasters pay gas on behalf of users)
//! - Batch operations (multiple calls in one UserOperation)
//!
//! # Architecture
//! - **UserOperation**: A pseudo-transaction describing what the user wants to do
//! - **Bundler**: Aggregates UserOperations and submits them as a single transaction
//! - **EntryPoint**: System program that validates and executes UserOperations
//! - **Paymaster**: Optional gas sponsor

use aether_agent_schema::{
    typed_hash, AgentAuthorization, PaymentEnvelope, PaymentToken, SideEffect, SigningAlgorithm,
};
use aether_crypto_primitives::verify as verify_ed25519;
use aether_crypto_threshold::verify_frost_ristretto255;
use aether_types::{Address, H256};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// Trait for account-specific signature validation.
///
/// Different accounts may use different signature schemes (multisig,
/// social recovery, passkeys, etc). The EntryPoint delegates to this
/// trait rather than hardcoding Ed25519.
pub trait AccountValidator {
    /// Validate that `signature` is a valid authorization for the
    /// operation identified by `op_hash`, sent by `sender`.
    fn validate_signature(&self, sender: &Address, op_hash: &H256, signature: &[u8]) -> Result<()>;
}

/// A user operation (ERC-4337 style pseudo-transaction).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserOperation {
    /// The account initiating this operation.
    pub sender: Address,
    /// Anti-replay nonce (managed by the account, not the protocol).
    pub nonce: u64,
    /// Calldata to execute on the sender's account.
    pub call_data: Vec<u8>,
    /// Gas limit for validation + execution.
    pub call_gas_limit: u64,
    /// Gas limit for verification step.
    pub verification_gas_limit: u64,
    /// Gas for pre-verification (bundler overhead).
    pub pre_verification_gas: u64,
    /// Maximum fee per gas unit.
    pub max_fee_per_gas: u128,
    /// Optional: paymaster address that sponsors gas.
    pub paymaster: Option<Address>,
    /// Optional: paymaster-specific data (approval signature, etc.).
    pub paymaster_data: Vec<u8>,
    /// Signature (validated by the account's custom logic, not Ed25519).
    pub signature: Vec<u8>,
    /// Declared side-effect class for policy and escrow validation.
    #[serde(default)]
    pub side_effect: Option<SideEffect>,
    /// Optional delegated agent session authorization.
    #[serde(default)]
    pub agent_authorization: Option<AgentAuthorization>,
    /// Optional AIC/SWR payment envelope bound to this operation.
    #[serde(default)]
    pub payment: Option<PaymentEnvelope>,
}

impl UserOperation {
    /// Hash the UserOperation for signing.
    ///
    /// Excludes the `signature` field so the hash is available before signing.
    pub fn hash(&self) -> H256 {
        typed_hash(
            "aether/user_operation/v1",
            &UserOperationSigningPayload {
                sender: self.sender,
                nonce: self.nonce,
                call_data: &self.call_data,
                call_gas_limit: self.call_gas_limit,
                verification_gas_limit: self.verification_gas_limit,
                pre_verification_gas: self.pre_verification_gas,
                max_fee_per_gas: self.max_fee_per_gas,
                paymaster: self.paymaster,
                paymaster_data: &self.paymaster_data,
                side_effect: self.side_effect,
                agent_authorization: self.agent_authorization.as_ref(),
                payment: self.payment.as_ref(),
            },
        )
        .expect("user operation signing payload serialization is infallible")
    }

    /// Hash the agent policy payload for guardian signing.
    ///
    /// This excludes the outer operation signature and all inner authorization
    /// or payment signatures, avoiding circular signatures while binding the
    /// guardian approval to the operation, payment terms, side effect, and
    /// authorization policy.
    pub fn agent_policy_hash(&self) -> H256 {
        typed_hash(
            "aether/user_operation_agent_policy/v1",
            &AgentPolicyPayload {
                sender: self.sender,
                nonce: self.nonce,
                call_data: &self.call_data,
                side_effect: self.side_effect,
                agent_authorization: self
                    .agent_authorization
                    .as_ref()
                    .map(AgentAuthorizationPolicyPayload::from),
                payment: self.payment.as_ref().map(PaymentPolicyPayload::from),
            },
        )
        .expect("agent policy payload serialization is infallible")
    }

    /// Total gas this operation requires.
    pub fn total_gas(&self) -> Result<u64> {
        self.call_gas_limit
            .checked_add(self.verification_gas_limit)
            .and_then(|g| g.checked_add(self.pre_verification_gas))
            .ok_or_else(|| anyhow::anyhow!("total gas overflow"))
    }

    /// Validate basic structural constraints.
    pub fn validate(&self) -> Result<()> {
        if self.call_gas_limit == 0 {
            bail!("call_gas_limit must be > 0");
        }
        if self.verification_gas_limit == 0 {
            bail!("verification_gas_limit must be > 0");
        }
        if self.max_fee_per_gas == 0 {
            bail!("max_fee_per_gas must be > 0");
        }
        if self.signature.is_empty() {
            bail!("signature must not be empty");
        }
        Ok(())
    }
}

impl Default for UserOperation {
    fn default() -> Self {
        Self {
            sender: Address::from([0u8; 20]),
            nonce: 0,
            call_data: Vec::new(),
            call_gas_limit: 0,
            verification_gas_limit: 0,
            pre_verification_gas: 0,
            max_fee_per_gas: 0,
            paymaster: None,
            paymaster_data: Vec::new(),
            signature: Vec::new(),
            side_effect: None,
            agent_authorization: None,
            payment: None,
        }
    }
}

/// EntryPoint manages UserOperation execution.
pub struct EntryPoint {
    /// Registered smart accounts: address → validation code hash.
    accounts: std::collections::HashMap<Address, H256>,
    /// Registered paymasters: address → deposit balance.
    paymasters: std::collections::HashMap<Address, u128>,
    /// Nonce tracking per account to prevent replay attacks.
    nonces: std::collections::HashMap<Address, u64>,
    /// Current slot used for session-key and payment expiration checks.
    current_slot: u64,
}

impl EntryPoint {
    pub fn new() -> Self {
        EntryPoint {
            accounts: std::collections::HashMap::new(),
            paymasters: std::collections::HashMap::new(),
            nonces: std::collections::HashMap::new(),
            current_slot: 0,
        }
    }

    /// Set the slot used for validating session-key and payment expiry.
    pub fn set_current_slot(&mut self, slot: u64) {
        self.current_slot = slot;
    }

    /// Register a smart account.
    pub fn register_account(&mut self, address: Address, code_hash: H256) {
        self.accounts.insert(address, code_hash);
    }

    /// Register a paymaster with an initial deposit.
    pub fn register_paymaster(&mut self, address: Address, deposit: u128) {
        self.paymasters.insert(address, deposit);
    }

    /// Validate a UserOperation before execution.
    pub fn validate_user_op(
        &self,
        op: &UserOperation,
        validator: &dyn AccountValidator,
    ) -> Result<()> {
        op.validate()?;

        // Check sender is a registered smart account
        if !self.accounts.contains_key(&op.sender) {
            bail!("sender {:?} is not a registered smart account", op.sender);
        }

        // Validate signature against the operation hash
        let op_hash = op.hash();
        validator.validate_signature(&op.sender, &op_hash, &op.signature)?;

        self.validate_agent_policy(op)?;

        // If paymaster is specified, check it's registered and has funds
        if let Some(paymaster) = &op.paymaster {
            let deposit = self
                .paymasters
                .get(paymaster)
                .ok_or_else(|| anyhow::anyhow!("paymaster not registered"))?;

            let required_gas_cost = (op.total_gas()? as u128)
                .checked_mul(op.max_fee_per_gas)
                .ok_or_else(|| anyhow::anyhow!("gas cost overflow"))?;
            if *deposit < required_gas_cost {
                bail!(
                    "paymaster deposit {} insufficient for gas cost {}",
                    deposit,
                    required_gas_cost
                );
            }
        }

        Ok(())
    }

    fn validate_agent_policy(&self, op: &UserOperation) -> Result<()> {
        let side_effect = op.side_effect.unwrap_or(SideEffect::Read);
        let high_risk = side_effect.requires_guardian();

        if high_risk && op.agent_authorization.is_none() {
            bail!("high-risk side effect requires agent authorization");
        }

        if let Some(auth) = &op.agent_authorization {
            auth.validate(self.current_slot)
                .map_err(|err| anyhow::anyhow!("agent authorization invalid: {err}"))?;

            if auth.agent_account != op.sender {
                bail!("agent authorization account does not match sender");
            }

            if !auth.allowed_side_effects.contains(&side_effect) {
                bail!("side effect {:?} is not authorized", side_effect);
            }

            if high_risk {
                if auth.signature.alg != SigningAlgorithm::FrostRistretto255 {
                    bail!("high-risk side effect requires FROST guardian signature");
                }
                let policy_hash = op.agent_policy_hash();
                auth.validate_guardian_signature_binding(policy_hash)
                    .map_err(|err| anyhow::anyhow!("guardian signature binding invalid: {err}"))?;
                let guardian_key = auth
                    .guardian_public_key
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("guardian public key missing"))?;
                verify_frost_ristretto255(
                    guardian_key,
                    policy_hash.as_bytes(),
                    &auth.signature.signature,
                )
                .map_err(|err| anyhow::anyhow!("guardian FROST verification failed: {err}"))?;
            }
        }

        if let Some(payment) = &op.payment {
            payment
                .validate(self.current_slot)
                .map_err(|err| anyhow::anyhow!("payment envelope invalid: {err}"))?;

            if payment.side_effect != side_effect {
                bail!("payment side effect does not match operation side effect");
            }

            let auth = op
                .agent_authorization
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("payment requires agent authorization"))?;
            self.validate_payment_signature(payment, auth)?;
            if payment.amount > auth.max_per_call_aic {
                bail!("payment exceeds per-call agent authorization cap");
            }
            if !auth.allowed_recipients.is_empty()
                && !auth.allowed_recipients.contains(&payment.recipient)
            {
                bail!("payment recipient is not authorized");
            }
        }

        Ok(())
    }

    fn validate_payment_signature(
        &self,
        payment: &PaymentEnvelope,
        auth: &AgentAuthorization,
    ) -> Result<()> {
        if payment.signature.alg != SigningAlgorithm::Ed25519 {
            bail!("payment signatures currently require Ed25519 session keys");
        }
        let payment_hash = payment
            .validate_signature_binding()
            .map_err(|err| anyhow::anyhow!("payment signature binding invalid: {err}"))?;
        verify_ed25519(
            &auth.session_public_key,
            payment_hash.as_bytes(),
            &payment.signature.signature,
        )
        .map_err(|err| anyhow::anyhow!("payment Ed25519 verification failed: {err}"))?;
        Ok(())
    }

    /// Execute a batch of UserOperations (bundler submission).
    pub fn handle_ops(
        &mut self,
        ops: &[UserOperation],
        validator: &dyn AccountValidator,
    ) -> Result<Vec<UserOpResult>> {
        let mut results = Vec::new();

        for op in ops {
            let result = match self.validate_user_op(op, validator) {
                Ok(()) => {
                    // Validate and increment nonce to prevent replay
                    let expected_nonce = self.nonces.get(&op.sender).copied().unwrap_or(0);
                    if op.nonce != expected_nonce {
                        results.push(UserOpResult {
                            op_hash: op.hash(),
                            success: false,
                            gas_used: op.verification_gas_limit,
                            error: Some(format!(
                                "invalid nonce: expected {}, got {}",
                                expected_nonce, op.nonce
                            )),
                        });
                        continue;
                    }
                    self.nonces.insert(
                        op.sender,
                        expected_nonce
                            .checked_add(1)
                            .ok_or_else(|| anyhow::anyhow!("nonce overflow"))?,
                    );

                    // Deduct paymaster deposit if applicable
                    if let Some(paymaster) = &op.paymaster {
                        let cost = (op.total_gas()? as u128)
                            .checked_mul(op.max_fee_per_gas)
                            .ok_or_else(|| anyhow::anyhow!("gas cost overflow"))?;
                        if let Some(deposit) = self.paymasters.get_mut(paymaster) {
                            if *deposit < cost {
                                return Err(anyhow::anyhow!(
                                    "insufficient paymaster deposit: have {}, need {}",
                                    deposit,
                                    cost
                                ));
                            }
                            *deposit = deposit
                                .checked_sub(cost)
                                .ok_or(anyhow::anyhow!("deposit underflow"))?;
                        }
                    }

                    UserOpResult {
                        op_hash: op.hash(),
                        success: true,
                        gas_used: op.call_gas_limit, // Simplified
                        error: None,
                    }
                }
                Err(e) => UserOpResult {
                    op_hash: op.hash(),
                    success: false,
                    gas_used: op.verification_gas_limit,
                    error: Some(e.to_string()),
                },
            };
            results.push(result);
        }

        Ok(results)
    }
}

#[derive(Serialize)]
struct UserOperationSigningPayload<'a> {
    sender: Address,
    nonce: u64,
    call_data: &'a [u8],
    call_gas_limit: u64,
    verification_gas_limit: u64,
    pre_verification_gas: u64,
    max_fee_per_gas: u128,
    paymaster: Option<Address>,
    paymaster_data: &'a [u8],
    side_effect: Option<SideEffect>,
    agent_authorization: Option<&'a AgentAuthorization>,
    payment: Option<&'a PaymentEnvelope>,
}

#[derive(Serialize)]
struct AgentPolicyPayload<'a> {
    sender: Address,
    nonce: u64,
    call_data: &'a [u8],
    side_effect: Option<SideEffect>,
    agent_authorization: Option<AgentAuthorizationPolicyPayload<'a>>,
    payment: Option<PaymentPolicyPayload>,
}

#[derive(Serialize)]
struct AgentAuthorizationPolicyPayload<'a> {
    agent_account: Address,
    session_public_key: &'a [u8],
    delegated_by: Address,
    valid_from_slot: u64,
    valid_until_slot: u64,
    max_aic: u128,
    max_per_call_aic: u128,
    allowed_side_effects: &'a [SideEffect],
    allowed_tools: &'a [String],
    allowed_recipients: &'a [Address],
    policy_hash: H256,
    guardian_threshold: Option<u16>,
    guardian_public_key: Option<&'a [u8]>,
}

impl<'a> From<&'a AgentAuthorization> for AgentAuthorizationPolicyPayload<'a> {
    fn from(auth: &'a AgentAuthorization) -> Self {
        Self {
            agent_account: auth.agent_account,
            session_public_key: &auth.session_public_key,
            delegated_by: auth.delegated_by,
            valid_from_slot: auth.valid_from_slot,
            valid_until_slot: auth.valid_until_slot,
            max_aic: auth.max_aic,
            max_per_call_aic: auth.max_per_call_aic,
            allowed_side_effects: &auth.allowed_side_effects,
            allowed_tools: &auth.allowed_tools,
            allowed_recipients: &auth.allowed_recipients,
            policy_hash: auth.policy_hash,
            guardian_threshold: auth.guardian_threshold,
            guardian_public_key: auth.guardian_public_key.as_deref(),
        }
    }
}

#[derive(Serialize)]
struct PaymentPolicyPayload {
    token: PaymentToken,
    amount: u128,
    recipient: Address,
    quote_hash: H256,
    request_hash: H256,
    result_hash: Option<H256>,
    nonce: [u8; 32],
    expires_at_slot: u64,
    chain_id: u64,
    side_effect: SideEffect,
    max_replays: u32,
    signature_payload_hash: H256,
}

impl From<&PaymentEnvelope> for PaymentPolicyPayload {
    fn from(payment: &PaymentEnvelope) -> Self {
        Self {
            token: payment.token,
            amount: payment.amount,
            recipient: payment.recipient,
            quote_hash: payment.quote_hash,
            request_hash: payment.request_hash,
            result_hash: payment.result_hash,
            nonce: payment.nonce,
            expires_at_slot: payment.expires_at_slot,
            chain_id: payment.chain_id,
            side_effect: payment.side_effect,
            max_replays: payment.max_replays,
            signature_payload_hash: payment.signature.payload_hash,
        }
    }
}

impl Default for EntryPoint {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of executing a UserOperation.
#[derive(Debug, Clone)]
pub struct UserOpResult {
    pub op_hash: H256,
    pub success: bool,
    pub gas_used: u64,
    pub error: Option<String>,
}
