use crate::primitives::{Address, H256};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Serde helper for u128 fields: serialize/deserialize as u64 in TOML since TOML
/// doesn't support u128. Values must fit in u64 range for TOML compatibility.
mod serde_u128_as_u64 {
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &u128, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let v = u64::try_from(*value).map_err(serde::ser::Error::custom)?;
        serializer.serialize_u64(v)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u128, D::Error>
    where
        D: Deserializer<'de>,
    {
        let v = u64::deserialize(deserializer)?;
        Ok(v as u128)
    }
}

// ---------------------------------------------------------------------------
// Chain ID (dual representation: human-readable + numeric for wire format)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainId {
    /// Human-readable name used in genesis hash computation and logging.
    pub name: String,
    /// Numeric ID used in transaction signing (EIP-155 style replay protection).
    pub numeric: u64,
}

// ---------------------------------------------------------------------------
// Top-level ChainConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainConfig {
    pub chain: ChainParams,
    pub consensus: ConsensusParams,
    pub fees: FeeParams,
    pub rent: RentParams,
    pub tokens: TokenParams,
    pub rewards: RewardParams,
    pub ai_mesh: AiMeshParams,
    pub networking: NetworkingParams,
}

// ---------------------------------------------------------------------------
// Sub-configs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainParams {
    /// Human-readable chain identifier (e.g. "aether-dev-1").
    pub chain_id: String,
    /// Numeric chain ID for transaction replay protection.
    pub chain_id_numeric: u64,
    /// Slot duration in milliseconds.
    pub slot_ms: u64,
    /// Maximum block size in bytes.
    pub block_bytes_max: u64,
    /// Number of slots per epoch.
    pub epoch_slots: u64,
    /// Number of epochs to retain old blocks and receipts before pruning.
    /// Set to 0 to disable pruning. Default: 10.
    #[serde(default = "default_retention_epochs")]
    pub retention_epochs: u64,
}

fn default_retention_epochs() -> u64 {
    10
}

impl ChainParams {
    pub fn chain_id(&self) -> ChainId {
        ChainId {
            name: self.chain_id.clone(),
            numeric: self.chain_id_numeric,
        }
    }

    pub fn slots_per_year(&self) -> u64 {
        (365 * 24 * 3600 * 1000) / self.slot_ms
    }

    pub fn epochs_per_year(&self) -> u64 {
        self.slots_per_year() / self.epoch_slots
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsensusParams {
    /// VRF leader election probability target.
    pub tau: f64,
    /// BFT quorum threshold as string fraction (e.g. "2/3").
    pub quorum: String,
    /// Slash percentage for double-signing (e.g. "0.05" = 5%).
    pub slash_double: String,
    /// Gradual leak rate per slot for downtime.
    pub leak_downtime: String,
    /// Unbonding delay in slots.
    pub unbonding_delay_slots: u64,
    /// HotStuff round timeout in ms.
    pub round_timeout_ms: u64,
    /// View change timeout in ms.
    pub view_change_timeout_ms: u64,
}

impl ConsensusParams {
    /// Parse quorum string "N/D" into (numerator, denominator).
    pub fn quorum_fraction(&self) -> Result<(u32, u32)> {
        let parts: Vec<&str> = self.quorum.split('/').collect();
        if parts.len() != 2 {
            bail!(
                "invalid quorum format: expected 'N/D', got '{}'",
                self.quorum
            );
        }
        let n: u32 = parts[0]
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid quorum numerator"))?;
        let d: u32 = parts[1]
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid quorum denominator"))?;
        if d == 0 {
            bail!("quorum denominator cannot be zero");
        }
        Ok((n, d))
    }

    /// Parse slash_double string to f64.
    pub fn slash_double_rate(&self) -> Result<f64> {
        self.slash_double
            .parse::<f64>()
            .map_err(|_| anyhow::anyhow!("invalid slash_double: '{}'", self.slash_double))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeParams {
    /// Base fee (lamports).
    #[serde(with = "serde_u128_as_u64")]
    pub a: u128,
    /// Per-byte fee.
    #[serde(with = "serde_u128_as_u64")]
    pub b: u128,
    /// Per compute step fee.
    #[serde(with = "serde_u128_as_u64")]
    pub c: u128,
    /// Per memory byte fee.
    #[serde(with = "serde_u128_as_u64")]
    pub d: u128,
    /// Congestion base multiplier.
    pub congestion_base: f64,
    /// Maximum congestion multiplier.
    pub congestion_max: f64,
    /// Target block utilization ratio.
    pub target_utilization: f64,
    /// Minimum base fee floor for EIP-1559 fee market.
    #[serde(with = "serde_u128_as_u64")]
    pub min_base_fee: u128,
    /// Base fee per blob (for blob transactions).
    #[serde(with = "serde_u128_as_u64")]
    pub blob_per_blob_fee: u128,
    /// Per-byte fee for blob data.
    #[serde(with = "serde_u128_as_u64")]
    pub blob_per_byte_fee: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RentParams {
    /// Rent per byte per epoch.
    pub rho_per_byte_per_epoch: u64,
    /// Prepaid deposit exemption horizon in epochs.
    pub horizon_epochs: u64,
    /// Minimum account balance.
    #[serde(with = "serde_u128_as_u64")]
    pub minimum_balance: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenParams {
    /// Initial SWR supply (staking/governance token).
    #[serde(with = "serde_u128_as_u64")]
    pub swr_initial_supply: u128,
    /// SWR decimal places.
    pub swr_decimals: u8,
    /// Initial AIC supply (AI credits token).
    #[serde(with = "serde_u128_as_u64")]
    pub aic_initial_supply: u128,
    /// AIC decimal places.
    pub aic_decimals: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewardParams {
    /// Annual inflation rate (e.g. 0.08 = 8%).
    pub annual_inflation_rate: f64,
    /// Maximum validator commission (e.g. 0.20 = 20%).
    pub validator_commission_max: f64,
    /// Reward distribution delay in epochs.
    pub reward_epoch_delay: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiMeshParams {
    /// VCR challenge window in slots.
    pub vcr_challenge_window_slots: u64,
    /// Minimum provider bond amount.
    #[serde(with = "serde_u128_as_u64")]
    pub vcr_bond_minimum: u128,
    /// EWMA decay factor for reputation scoring.
    pub reputation_decay_alpha: f64,
    /// Number of KZG opening samples.
    pub kzg_sample_size: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkingParams {
    /// Maximum number of peers.
    pub max_peers: u32,
    /// Maximum inbound connections.
    pub max_inbound: u32,
    /// Maximum outbound connections.
    pub max_outbound: u32,
    /// Gossipsub mesh size.
    pub gossipsub_mesh_size: u32,
    /// Turbine fanout.
    pub turbine_fanout: u32,
    /// Reed-Solomon data shards.
    pub erasure_k: u32,
    /// Reed-Solomon parity shards.
    pub erasure_r: u32,
}

// ---------------------------------------------------------------------------
// Well-known program/contract addresses
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct WellKnownAddresses {
    pub transfer_program: H256,
    pub staking_delegate: Address,
    pub staking_withdraw: Address,
}

impl WellKnownAddresses {
    pub fn default_addresses() -> Self {
        WellKnownAddresses {
            transfer_program: H256([1u8; 32]),
            staking_delegate: Address::from_slice(&{
                let mut bytes = [0u8; 20];
                bytes[19] = 0xab;
                bytes
            })
            .unwrap(),
            staking_withdraw: Address::from_slice(&{
                let mut bytes = [0u8; 20];
                bytes[19] = 0xac;
                bytes
            })
            .unwrap(),
        }
    }
}

// ---------------------------------------------------------------------------
// Loading & Validation
// ---------------------------------------------------------------------------

impl ChainConfig {
    /// Load config from a TOML file.
    pub fn from_toml_file(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read config file {}: {}", path.display(), e))?;
        Self::from_toml_str(&contents)
    }

    /// Parse config from a TOML string.
    pub fn from_toml_str(s: &str) -> Result<Self> {
        let config: ChainConfig =
            toml::from_str(s).map_err(|e| anyhow::anyhow!("failed to parse config TOML: {}", e))?;
        config.validate()?;
        Ok(config)
    }

    /// Validate all config invariants.
    pub fn validate(&self) -> Result<()> {
        // Chain params
        if self.chain.chain_id.is_empty() {
            bail!("chain_id must not be empty");
        }
        if self.chain.slot_ms == 0 {
            bail!("slot_ms must be > 0");
        }
        if self.chain.epoch_slots == 0 {
            bail!("epoch_slots must be > 0");
        }
        if self.chain.block_bytes_max == 0 {
            bail!("block_bytes_max must be > 0");
        }

        // Consensus params
        if !(0.0..=1.0).contains(&self.consensus.tau) {
            bail!("tau must be in [0.0, 1.0], got {}", self.consensus.tau);
        }
        self.consensus.quorum_fraction()?;
        self.consensus.slash_double_rate()?;

        // Fee params
        if self.fees.target_utilization <= 0.0 || self.fees.target_utilization > 1.0 {
            bail!(
                "target_utilization must be in (0.0, 1.0], got {}",
                self.fees.target_utilization
            );
        }

        // Token params
        if self.tokens.swr_initial_supply == 0 {
            bail!("swr_initial_supply must be > 0");
        }

        // Networking params
        let (n, d) = self.consensus.quorum_fraction()?;
        if n == 0 || n > d {
            bail!("quorum {}/{} is invalid", n, d);
        }

        Ok(())
    }

    /// Well-known addresses for this chain.
    pub fn well_known_addresses(&self) -> WellKnownAddresses {
        WellKnownAddresses::default_addresses()
    }

    // -----------------------------------------------------------------------
    // Network presets
    // -----------------------------------------------------------------------

    /// Devnet preset -- matches config/genesis.toml defaults.
    pub fn devnet() -> Self {
        ChainConfig {
            chain: ChainParams {
                chain_id: "aether-dev-1".into(),
                chain_id_numeric: 900,
                slot_ms: 500,
                block_bytes_max: 2_000_000,
                epoch_slots: 43_200,
                retention_epochs: 10,
            },
            consensus: ConsensusParams {
                tau: 0.8,
                quorum: "2/3".into(),
                slash_double: "0.05".into(),
                leak_downtime: "0.00001".into(),
                unbonding_delay_slots: 172_800,
                round_timeout_ms: 2000,
                view_change_timeout_ms: 5000,
            },
            fees: FeeParams {
                a: 10_000,
                b: 5,
                c: 2,
                d: 1,
                congestion_base: 1.0,
                congestion_max: 100.0,
                target_utilization: 0.75,
                min_base_fee: 1_000,
                blob_per_blob_fee: 100_000,
                blob_per_byte_fee: 1,
            },
            rent: RentParams {
                rho_per_byte_per_epoch: 2,
                horizon_epochs: 12,
                minimum_balance: 1_000_000,
            },
            tokens: TokenParams {
                swr_initial_supply: 1_000_000_000_000_000,
                swr_decimals: 6,
                aic_initial_supply: 10_000_000_000_000_000,
                aic_decimals: 6,
            },
            rewards: RewardParams {
                annual_inflation_rate: 0.08,
                validator_commission_max: 0.20,
                reward_epoch_delay: 2,
            },
            ai_mesh: AiMeshParams {
                vcr_challenge_window_slots: 1200,
                vcr_bond_minimum: 10_000_000,
                reputation_decay_alpha: 0.95,
                kzg_sample_size: 32,
            },
            networking: NetworkingParams {
                max_peers: 50,
                max_inbound: 25,
                max_outbound: 25,
                gossipsub_mesh_size: 8,
                turbine_fanout: 12,
                erasure_k: 10,
                erasure_r: 2,
            },
        }
    }

    /// Testnet preset.
    pub fn testnet() -> Self {
        let mut config = Self::devnet();
        config.chain.chain_id = "aether-testnet-1".into();
        config.chain.chain_id_numeric = 100;
        config
    }

    /// Mainnet preset (stricter parameters).
    pub fn mainnet() -> Self {
        let mut config = Self::devnet();
        config.chain.chain_id = "aether-mainnet-1".into();
        config.chain.chain_id_numeric = 1;
        // Mainnet: larger epoch, more conservative fees
        config.chain.epoch_slots = 86_400; // ~12 hours
        config.consensus.unbonding_delay_slots = 345_600; // 48 hours
        config
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
