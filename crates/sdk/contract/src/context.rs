/// Execution context available to smart contracts.
///
/// Provides information about the current block, caller, and contract.
#[derive(Debug, Clone)]
pub struct ContractContext {
    pub caller: [u8; 20],
    pub contract_address: [u8; 20],
    pub block_number: u64,
    pub timestamp: u64,
    pub value: u128,
}

impl ContractContext {
    /// Get the caller's address as a hex string.
    pub fn caller_hex(&self) -> String {
        hex_encode(&self.caller)
    }

    /// Get the contract's address as a hex string.
    pub fn address_hex(&self) -> String {
        hex_encode(&self.contract_address)
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}
