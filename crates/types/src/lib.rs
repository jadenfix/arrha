// ============================================================================
// AETHER TYPES - Canonical Type Definitions
// ============================================================================
// PURPOSE: Shared types used across all Aether components
//
// CORE TYPES:
// - H256: 32-byte hash
// - Address: Account address (20 bytes)
// - Signature: Cryptographic signature
// - Block, Transaction, UTxO, Account
// - Slot, Epoch
//
// All types implement:
// - Serialize/Deserialize (serde)
// - Clone, Debug
// - Consistent encoding (for hashing)
// ============================================================================

pub mod account;
pub mod block;
pub mod chain_config;
pub mod consensus;
pub mod primitives;
pub mod transaction;

pub use account::{Account, Utxo};
pub use block::{
    AggregatedVote, Block, BlockHeader, SlashEvidence, SlashEvidenceType, SlashVote, VrfProof,
    PROTOCOL_VERSION,
};
pub use chain_config::{
    AiMeshParams, ChainConfig, ChainId, ChainParams, ConsensusParams, FeeParams, NetworkingParams,
    RentParams, RewardParams, TokenParams, WellKnownAddresses,
};
pub use consensus::{EpochInfo, ValidatorInfo, Vote};
pub use primitives::{Address, Epoch, PublicKey, Signature, Slot, H160, H256};

pub use transaction::{
    contract_address, BlobTransaction, CallPayload, DeployPayload, Transaction, TransactionReceipt,
    TransactionStatus, TransferPayload, UtxoId, UtxoOutput, BLOB_RETENTION_SLOTS, CALL_PROGRAM_ID,
    DEPLOY_PROGRAM_ID, MAX_BLOBS_PER_TX, MAX_BLOB_SIZE, TRANSFER_PROGRAM_ID,
};
