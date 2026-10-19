// ============================================================================
// AETHER LEDGER - eUTxO++ State Management
// ============================================================================
// PURPOSE: Hybrid UTxO + account model with Sparse Merkle commitment
// ============================================================================

pub mod emission;
pub mod fee_market;
pub mod state;

pub use emission::EmissionSchedule;
pub use fee_market::FeeMarket;
pub use state::{
    BlockContext, ContractCall, ContractExecutor, ContractOutcome, Ledger, PendingOverlay,
    MAX_CONTRACT_CODE_BYTES,
};

pub use aether_state_merkle::SparseMerkleTree;
