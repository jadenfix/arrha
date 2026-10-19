use aether_types::{Address, H256};
use anyhow::Result;
use std::collections::HashMap;

/// Execution context injected into host functions for each contract call.
#[derive(Debug, Clone)]
pub struct ExecutionContext {
    pub block_number: u64,
    pub timestamp: u64,
    pub caller: Address,
    pub contract_address: Address,
}

impl Default for ExecutionContext {
    fn default() -> Self {
        ExecutionContext {
            block_number: 0,
            timestamp: 0,
            caller: Address::from([0u8; 20]),
            contract_address: Address::from([0u8; 20]),
        }
    }
}

/// Host Functions for WASM Contracts
///
/// These functions are imported into the WASM environment and allow
/// contracts to interact with the blockchain state.
///
/// Security:
/// - All functions charge gas
/// - Memory access is bounds-checked
/// - State changes are atomic
/// - No access to host filesystem/network
///
pub struct HostFunctions {
    /// Contract storage (key -> value)
    storage: HashMap<Vec<u8>, Vec<u8>>,

    /// Account balances
    pub balances: HashMap<Address, u128>,

    /// Gas meter
    gas_used: u64,
    gas_limit: u64,

    /// Execution context (block number, timestamp, caller, etc.)
    context: ExecutionContext,
}

impl HostFunctions {
    /// Create with execution context.
    pub fn new(gas_limit: u64, context: ExecutionContext) -> Self {
        HostFunctions {
            storage: HashMap::new(),
            balances: HashMap::new(),
            gas_used: 0,
            gas_limit,
            context,
        }
    }

    /// Create with default/zeroed context (convenience for tests).
    pub fn new_for_test(gas_limit: u64) -> Self {
        Self::new(gas_limit, ExecutionContext::default())
    }

    /// Read from contract storage
    /// Cost: 200 gas
    pub fn storage_read(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.charge_gas(200)?;
        Ok(self.storage.get(key).cloned())
    }

    /// Write to contract storage
    /// Cost: 5000 gas (expensive to incentivize minimal storage)
    pub fn storage_write(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        self.charge_gas(5000)?;

        // Charge extra for new keys
        if !self.storage.contains_key(&key) {
            self.charge_gas(20000)?; // New storage slot
        }

        self.storage.insert(key, value);
        Ok(())
    }

    /// Get account balance
    /// Cost: 100 gas
    pub fn get_balance(&mut self, address: &Address) -> Result<u128> {
        self.charge_gas(100)?;
        Ok(self.balances.get(address).copied().unwrap_or(0))
    }

    /// Transfer value to another account
    /// Cost: 9000 gas
    pub fn transfer(&mut self, from: &Address, to: &Address, amount: u128) -> Result<()> {
        self.charge_gas(9000)?;

        let from_balance = self.balances.get(from).copied().unwrap_or(0);
        if from_balance < amount {
            anyhow::bail!("insufficient balance");
        }

        let to_balance = self.balances.get(to).copied().unwrap_or(0);

        self.balances.insert(
            *from,
            from_balance
                .checked_sub(amount)
                .ok_or_else(|| anyhow::anyhow!("balance underflow"))?,
        );
        let new_to_balance = to_balance
            .checked_add(amount)
            .ok_or_else(|| anyhow::anyhow!("balance overflow"))?;
        self.balances.insert(*to, new_to_balance);

        Ok(())
    }

    /// Compute SHA256 hash
    /// Cost: 60 gas + 12 gas per word
    #[allow(clippy::manual_div_ceil)]
    pub fn sha256(&mut self, data: &[u8]) -> Result<H256> {
        let words = (data.len() + 31) / 32;
        self.charge_gas(60u64.saturating_add(12u64.saturating_mul(words as u64)))?;

        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(data);
        Ok(H256::from(<[u8; 32]>::from(hash)))
    }

    /// Emit a log event
    /// Cost: 375 gas + 8 gas per byte
    pub fn emit_log(&mut self, topics: Vec<H256>, data: Vec<u8>) -> Result<()> {
        self.charge_gas(375u64.saturating_add(8u64.saturating_mul(data.len() as u64)))?;

        // In production: store logs for receipts
        tracing::debug!(topics = ?topics, data_len = data.len(), "contract log emitted");

        Ok(())
    }

    /// Get current block number
    /// Cost: 2 gas
    pub fn block_number(&mut self) -> Result<u64> {
        self.charge_gas(2)?;
        Ok(self.context.block_number)
    }

    /// Get current timestamp
    /// Cost: 2 gas
    pub fn timestamp(&mut self) -> Result<u64> {
        self.charge_gas(2)?;
        Ok(self.context.timestamp)
    }

    /// Get caller address
    /// Cost: 2 gas
    pub fn caller(&mut self) -> Result<Address> {
        self.charge_gas(2)?;
        Ok(self.context.caller)
    }

    /// Get contract address
    /// Cost: 2 gas
    pub fn address(&mut self) -> Result<Address> {
        self.charge_gas(2)?;
        Ok(self.context.contract_address)
    }

    fn charge_gas(&mut self, amount: u64) -> Result<()> {
        self.gas_used = self
            .gas_used
            .checked_add(amount)
            .ok_or_else(|| anyhow::anyhow!("gas overflow"))?;

        if self.gas_used > self.gas_limit {
            anyhow::bail!("out of gas");
        }

        Ok(())
    }

    pub fn gas_used(&self) -> u64 {
        self.gas_used
    }
}
