use std::collections::HashSet;

use aether_crypto_primitives::Keypair;
use aether_types::{
    Address, PublicKey, Signature, Transaction, TransferPayload, TRANSFER_PROGRAM_ID,
};

use crate::error::AetherSdkError;
use crate::types::ClientConfig;

/// Builder for constructing token transfer transactions.
pub struct TransferBuilder {
    recipient: Option<Address>,
    amount: Option<u128>,
    memo: Option<String>,
    fee: u128,
    gas_limit: u64,
    chain_id: u64,
}

impl TransferBuilder {
    pub(crate) fn new(config: &ClientConfig) -> Self {
        TransferBuilder {
            recipient: None,
            amount: None,
            memo: None,
            fee: config.default_fee,
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
        self.fee = fee;
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
        let recipient = self
            .recipient
            .ok_or_else(|| AetherSdkError::build("missing recipient"))?;
        let amount = self
            .amount
            .ok_or_else(|| AetherSdkError::build("missing amount"))?;

        if amount == 0 {
            return Err(AetherSdkError::build(
                "transfer amount must be greater than zero",
            ));
        }

        let payload = TransferPayload {
            recipient,
            amount,
            memo: self.memo,
        };

        let payload_bytes = bincode::serialize(&payload).map_err(AetherSdkError::serialization)?;
        let sender_pubkey = PublicKey::from_bytes(keypair.public_key());
        let sender_address = sender_pubkey.to_address();

        let mut writes = HashSet::new();
        writes.insert(recipient);
        writes.insert(sender_address);
        let reads = HashSet::from([sender_address, recipient]);

        let mut tx = Transaction {
            nonce,
            chain_id: self.chain_id,
            sender: sender_address,
            sender_pubkey,
            inputs: Vec::new(),
            outputs: Vec::new(),
            reads,
            writes,
            program_id: Some(TRANSFER_PROGRAM_ID),
            data: payload_bytes,
            gas_limit: self.gas_limit,
            fee: self.fee,
            signature: Signature::from_bytes(vec![0; 64]),
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
        let fee_params = aether_types::ChainConfig::devnet().fees;
        tx.calculate_fee(&fee_params)
            .map_err(|e| AetherSdkError::InvalidFee(e.to_string()))?;
        Ok(tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_transfer_uses_canonical_ledger_program_and_access_sets() {
        let key = Keypair::generate();
        let recipient = Address::from_slice(&[7u8; 20]).unwrap();
        let tx = TransferBuilder::new(&ClientConfig::default())
            .to(recipient)
            .amount(1000)
            .memo("actual transfer")
            .chain_id(900)
            .build(&key, 0)
            .unwrap();
        assert_eq!(tx.program_id, Some(TRANSFER_PROGRAM_ID));
        let payload: TransferPayload = bincode::deserialize(&tx.data).unwrap();
        assert_eq!(payload.recipient, recipient);
        assert_eq!(payload.amount, 1000);
        assert_eq!(payload.memo.as_deref(), Some("actual transfer"));
        assert!(tx.reads.contains(&tx.sender) && tx.reads.contains(&recipient));
        assert!(tx.writes.contains(&tx.sender) && tx.writes.contains(&recipient));
        tx.verify_signature().unwrap();
        assert_eq!(tx.chain_id, 900);
    }

    #[test]
    fn zero_transfer_rejected_before_signing() {
        let key = Keypair::generate();
        let recipient = Address::from_slice(&[7u8; 20]).unwrap();
        assert!(TransferBuilder::new(&ClientConfig::default())
            .to(recipient)
            .amount(0)
            .build(&key, 0)
            .is_err());
    }
}
