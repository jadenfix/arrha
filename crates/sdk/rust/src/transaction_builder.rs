use std::collections::HashSet;

use aether_crypto_primitives::Keypair;
use aether_types::{Address, ChainConfig, PublicKey, Signature, Transaction};

use crate::chain::{recommended_fee_for_transaction, ChainFeeParameters};
use crate::error::AetherSdkError;
use crate::types::{ClientConfig, TransferRequest};

/// Builder for constructing token transfer transactions.
pub struct TransferBuilder {
    recipient: Option<Address>,
    amount: Option<u128>,
    memo: Option<String>,
    fee: Option<u128>,
    default_fee: u128,
    gas_limit: u64,
    chain_id: u64,
}

impl TransferBuilder {
    pub(crate) fn new(config: &ClientConfig) -> Self {
        TransferBuilder {
            recipient: None,
            amount: None,
            memo: None,
            fee: None,
            default_fee: config.default_fee,
            gas_limit: config.default_gas_limit,
            chain_id: 1,
        }
    }

    /// Set the recipient address.
    pub fn to(mut self, recipient: Address) -> Self {
        self.recipient = Some(recipient);
        self
    }

    /// Set the transfer amount in base units.
    pub fn amount(mut self, amount: u128) -> Self {
        self.amount = Some(amount);
        self
    }

    /// Attach an optional memo string to the transfer.
    pub fn memo<T: Into<String>>(mut self, memo: T) -> Self {
        self.memo = Some(memo.into());
        self
    }

    /// Override the default transaction fee.
    pub fn fee(mut self, fee: u128) -> Self {
        self.fee = Some(fee);
        self
    }

    /// Override the default gas limit.
    pub fn gas_limit(mut self, gas_limit: u64) -> Self {
        self.gas_limit = gas_limit;
        self
    }

    /// Set the chain ID (defaults to 1).
    pub fn chain_id(mut self, chain_id: u64) -> Self {
        self.chain_id = chain_id;
        self
    }

    /// Build and sign the transfer transaction.
    pub fn build(self, keypair: &Keypair, nonce: u64) -> Result<Transaction, AetherSdkError> {
        let fees = ChainConfig::devnet().fees;
        let parameters = ChainFeeParameters {
            a: fees.a,
            b: fees.b,
            c: fees.c,
            min_base_fee: fees.min_base_fee,
            chain_id: self.chain_id,
        };
        let fallback_fee = self.default_fee;
        self.build_using_parameters(keypair, nonce, &parameters, Some(fallback_fee))
    }

    /// Build and sign a transfer using fee coefficients fetched from the node.
    pub fn build_with_fee_parameters(
        self,
        keypair: &Keypair,
        nonce: u64,
        fees: &ChainFeeParameters,
    ) -> Result<Transaction, AetherSdkError> {
        self.build_using_parameters(keypair, nonce, fees, None)
    }

    fn build_using_parameters(
        self,
        keypair: &Keypair,
        nonce: u64,
        fees: &ChainFeeParameters,
        default_fee: Option<u128>,
    ) -> Result<Transaction, AetherSdkError> {
        let recipient = self
            .recipient
            .ok_or_else(|| AetherSdkError::build("missing recipient"))?;
        let amount = self
            .amount
            .ok_or_else(|| AetherSdkError::build("missing amount"))?;

        let payload = TransferRequest {
            recipient,
            amount,
            memo: self.memo,
        };

        let payload_bytes = bincode::serialize(&payload).map_err(AetherSdkError::serialization)?;
        let sender_pubkey = PublicKey::from_bytes(keypair.public_key());
        let sender_address = sender_pubkey.to_address();

        let mut writes = HashSet::new();
        writes.insert(recipient);

        let mut tx = Transaction {
            nonce,
            chain_id: self.chain_id,
            sender: sender_address,
            sender_pubkey,
            inputs: Vec::new(),
            outputs: Vec::new(),
            reads: HashSet::new(),
            writes,
            program_id: None,
            data: payload_bytes,
            gas_limit: self.gas_limit,
            fee: 0,
            signature: Signature::from_bytes(vec![0; 64]),
        };

        tx.fee = match self.fee.or(default_fee) {
            Some(fee) => fee,
            None => recommended_fee_for_transaction(&tx, fees)?,
        };

        let message = tx.hash();
        let signature = keypair.sign(message.as_bytes());
        if signature.len() != 64 {
            return Err(AetherSdkError::InvalidSignature(format!(
                "invalid signature length: {}",
                signature.len()
            )));
        }
        tx.signature = Signature::from_bytes(signature);
        tx.verify_signature()
            .map_err(|e| AetherSdkError::InvalidSignature(e.to_string()))?;
        if tx.chain_id != fees.chain_id {
            return Err(AetherSdkError::InvalidFee(
                "fee parameter chain id mismatch".into(),
            ));
        }
        let minimum = crate::chain::minimum_fee_for_validation(&tx, fees)?;
        if tx.fee < minimum {
            return Err(AetherSdkError::InvalidFee(format!(
                "fee too low: provided {}, required {}",
                tx.fee, minimum
            )));
        }
        Ok(tx)
    }
}
