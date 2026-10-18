use aether_types::{Address, Block, ChainConfig, PublicKey, ValidatorInfo, VrfProof, H256};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::hybrid_node::ValidatorKeypair;

/// Genesis configuration for bootstrapping a new network.
///
/// Embeds the full `ChainConfig` (consensus, fees, networking, etc.)
/// alongside the initial validator set and account allocations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenesisConfig {
    /// Full chain configuration (loaded from genesis.toml).
    #[serde(flatten)]
    pub chain_config: ChainConfig,
    /// Initial validators with their stakes.
    #[serde(default)]
    pub validators: Vec<GenesisValidator>,
    /// Initial account balances.
    #[serde(default)]
    pub accounts: Vec<GenesisAccount>,
    /// Genesis timestamp (unix seconds).
    #[serde(default)]
    pub timestamp: u64,
    /// Unix milliseconds at which slot 0 starts. All validators derive the
    /// current slot as `(now_ms - genesis_time_ms) / slot_ms`, which keeps
    /// independent processes on the same slot. 0 = use `timestamp * 1000`.
    #[serde(default)]
    pub genesis_time_ms: u64,
    /// Initial protocol version.
    #[serde(default = "default_protocol_version")]
    pub protocol_version: u32,
}

fn default_protocol_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenesisValidator {
    pub name: String,
    /// Ed25519 public key (32 bytes) — used for transaction signing and address derivation.
    pub pubkey: Vec<u8>,
    /// BLS public key (48 bytes) — used for consensus vote signing.
    #[serde(default)]
    pub bls_pubkey: Vec<u8>,
    /// BLS proof-of-possession signature — proves ownership of the BLS key.
    #[serde(default)]
    pub bls_pop: Vec<u8>,
    /// VRF public key (32 bytes) — used for leader election.
    #[serde(default)]
    pub vrf_pubkey: [u8; 32],
    pub stake: u128,
    pub commission_bps: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenesisAccount {
    pub address: Address,
    pub balance: u128,
}

/// Result of building genesis state.
pub struct GenesisResult {
    pub genesis_block: Block,
    pub genesis_hash: H256,
    pub validator_set: Vec<ValidatorInfo>,
    pub total_stake: u128,
    pub total_supply: u128,
}

impl GenesisConfig {
    /// Create a genesis config from a ChainConfig preset with the given validators/accounts.
    pub fn new(
        chain_config: ChainConfig,
        validators: Vec<GenesisValidator>,
        accounts: Vec<GenesisAccount>,
    ) -> Self {
        GenesisConfig {
            chain_config,
            validators,
            accounts,
            timestamp: 0,
            genesis_time_ms: 0,
            protocol_version: 1,
        }
    }

    /// Build the genesis block and initial state from the config.
    pub fn build(&self) -> GenesisResult {
        // Build validator set
        let validator_set: Vec<ValidatorInfo> = self
            .validators
            .iter()
            .map(|v| ValidatorInfo {
                pubkey: PublicKey::from_bytes(v.pubkey.clone()),
                stake: v.stake,
                commission: v.commission_bps,
                active: true,
            })
            .collect();

        let total_stake: u128 = validator_set
            .iter()
            .fold(0u128, |acc, v| acc.saturating_add(v.stake));
        let total_supply: u128 = self
            .accounts
            .iter()
            .fold(0u128, |acc, a| acc.saturating_add(a.balance))
            .saturating_add(total_stake);

        // Compute genesis state root from accounts
        let state_root = self.compute_state_root();

        // Create genesis block (slot 0, no parent)
        let genesis_block = Block {
            header: aether_types::BlockHeader {
                version: self.protocol_version,
                slot: 0,
                parent_hash: H256::zero(),
                state_root,
                transactions_root: H256::zero(),
                receipts_root: H256::zero(),
                proposer: Address::from([0u8; 20]),
                vrf_proof: VrfProof {
                    output: [0u8; 32],
                    proof: vec![],
                },
                timestamp: self.timestamp,
            },
            transactions: vec![],
            aggregated_vote: None,
            slash_evidence: Vec::new(),
        };

        let genesis_hash = genesis_block.hash();

        GenesisResult {
            genesis_block,
            genesis_hash,
            validator_set,
            total_stake,
            total_supply,
        }
    }

    fn compute_state_root(&self) -> H256 {
        let mut hasher = Sha256::new();
        hasher.update(self.chain_config.chain.chain_id.as_bytes());
        for account in &self.accounts {
            hasher.update(account.address.as_bytes());
            hasher.update(account.balance.to_le_bytes());
        }
        for validator in &self.validators {
            hasher.update(&validator.pubkey);
            hasher.update(validator.stake.to_le_bytes());
        }
        H256::from(<[u8; 32]>::from(hasher.finalize()))
    }

    /// Extract VRF public keys for all validators (for consensus registration).
    pub fn vrf_pubkeys(&self) -> Vec<(Address, [u8; 32])> {
        self.validators
            .iter()
            .filter(|v| v.vrf_pubkey != [0u8; 32])
            .map(|v| {
                let addr = PublicKey::from_bytes(v.pubkey.clone()).to_address();
                (addr, v.vrf_pubkey)
            })
            .collect()
    }

    /// Extract BLS public keys + PoP for all validators (for consensus registration).
    pub fn bls_pubkeys(&self) -> Vec<(Address, Vec<u8>, Vec<u8>)> {
        self.validators
            .iter()
            .filter(|v| !v.bls_pubkey.is_empty())
            .map(|v| {
                let addr = PublicKey::from_bytes(v.pubkey.clone()).to_address();
                (addr, v.bls_pubkey.clone(), v.bls_pop.clone())
            })
            .collect()
    }

    /// Create a genesis config from a set of validator keypairs.
    /// This is the primary way to bootstrap a new devnet/testnet.
    pub fn from_keypairs(
        chain_config: ChainConfig,
        keypairs: &[ValidatorKeypair],
        stake_per_validator: u128,
    ) -> Self {
        let validators: Vec<GenesisValidator> = keypairs
            .iter()
            .enumerate()
            .map(|(i, kp)| GenesisValidator {
                name: format!("validator-{}", i + 1),
                pubkey: kp.ed25519.public_key(),
                bls_pubkey: kp.bls.public_key(),
                bls_pop: kp.bls.proof_of_possession(),
                vrf_pubkey: *kp.vrf.public_key(),
                stake: stake_per_validator,
                commission_bps: 500, // 5% default
            })
            .collect();

        let accounts: Vec<GenesisAccount> = keypairs
            .iter()
            .map(|kp| GenesisAccount {
                address: kp.address(),
                balance: stake_per_validator * 10, // 10x stake as initial balance
            })
            .collect();

        GenesisConfig {
            chain_config,
            validators,
            accounts,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            genesis_time_ms: 0,
            protocol_version: 1,
        }
    }

    /// Unix-ms at which slot 0 starts (`genesis_time_ms`, else `timestamp`).
    pub fn genesis_time_ms(&self) -> Option<u64> {
        if self.genesis_time_ms > 0 {
            Some(self.genesis_time_ms)
        } else if self.timestamp > 0 {
            Some(self.timestamp.saturating_mul(1000))
        } else {
            None
        }
    }

    /// Validate the genesis config.
    pub fn validate(&self) -> Result<()> {
        self.chain_config.validate()?;

        if self.validators.is_empty() {
            bail!("must have at least one validator");
        }
        for v in &self.validators {
            if v.stake == 0 {
                bail!("validator {} has zero stake", v.name);
            }
            if v.pubkey.is_empty() {
                bail!("validator {} has empty pubkey", v.name);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> GenesisConfig {
        GenesisConfig {
            chain_config: ChainConfig::devnet(),
            validators: vec![
                GenesisValidator {
                    name: "validator-1".into(),
                    pubkey: vec![1u8; 32],
                    bls_pubkey: vec![],
                    bls_pop: vec![],
                    vrf_pubkey: [0u8; 32],
                    stake: 1_000_000,
                    commission_bps: 500,
                },
                GenesisValidator {
                    name: "validator-2".into(),
                    pubkey: vec![2u8; 32],
                    bls_pubkey: vec![],
                    bls_pop: vec![],
                    vrf_pubkey: [0u8; 32],
                    stake: 2_000_000,
                    commission_bps: 300,
                },
            ],
            accounts: vec![GenesisAccount {
                address: Address::from_slice(&[10u8; 20]).unwrap(),
                balance: 100_000_000,
            }],
            timestamp: 1700000000,
            genesis_time_ms: 0,
            protocol_version: 1,
        }
    }

    #[test]
    fn test_genesis_build() {
        let config = test_config();
        let result = config.build();

        assert_eq!(result.genesis_block.header.slot, 0);
        assert_eq!(result.genesis_block.header.version, 1);
        assert_eq!(result.validator_set.len(), 2);
        assert_eq!(result.total_stake, 3_000_000);
        assert_ne!(result.genesis_hash, H256::zero());
    }

    #[test]
    fn test_genesis_deterministic() {
        let config = test_config();
        let r1 = config.build();
        let r2 = config.build();
        assert_eq!(r1.genesis_hash, r2.genesis_hash);
    }

    #[test]
    fn test_genesis_validation() {
        let config = test_config();
        assert!(config.validate().is_ok());

        let mut bad = test_config();
        bad.validators.clear();
        assert!(bad.validate().is_err());

        let mut bad2 = test_config();
        bad2.chain_config.chain.chain_id = String::new();
        assert!(bad2.validate().is_err());
    }

    #[test]
    fn test_state_root_changes_with_accounts() {
        let c1 = test_config();
        let mut c2 = test_config();
        c2.accounts[0].balance = 999;

        let r1 = c1.build();
        let r2 = c2.build();
        assert_ne!(
            r1.genesis_block.header.state_root,
            r2.genesis_block.header.state_root
        );
    }
}
