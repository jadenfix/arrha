//! Genesis ceremony tool: generates validator keys and a shared genesis.json
//! for bootstrapping a multi-validator devnet.
//!
//! Usage:
//!   genesis-ceremony --validators 4 --output-dir /data/genesis
//!
//! Produces:
//!   /data/genesis/validator-1.key
//!   /data/genesis/validator-2.key
//!   ...
//!   /data/genesis/genesis.json
//!
//! Also funds `--dev-accounts` deterministic devnet accounts
//! (`Keypair::devnet(i)`, publicly derivable; local networks only) and sets
//! `genesis_time_ms` a few seconds in the future so every validator process
//! derives the same slot from the wall clock.

use aether_crypto_primitives::Keypair;
use aether_node::genesis::GenesisAccount;
use aether_node::{GenesisConfig, ValidatorKeypair};
use aether_types::{Address, ChainConfig};
use anyhow::{Context, Result};
use std::path::PathBuf;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let mut num_validators: usize = 4;
    let mut output_dir = PathBuf::from(".");
    let mut stake_per_validator: u128 = 1_000_000;
    let mut network = "devnet".to_string();
    let mut dev_accounts: u32 = 256;
    let mut dev_balance: u128 = 1_000_000_000_000_000;
    let mut genesis_delay_ms: u64 = 3_000;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--validators" | "-n" => {
                i += 1;
                num_validators = args[i].parse().context("invalid --validators value")?;
            }
            "--output-dir" | "-o" => {
                i += 1;
                output_dir = PathBuf::from(&args[i]);
            }
            "--stake" => {
                i += 1;
                stake_per_validator = args[i].parse().context("invalid --stake value")?;
            }
            "--network" => {
                i += 1;
                network = args[i].clone();
            }
            "--dev-accounts" => {
                i += 1;
                dev_accounts = args[i].parse().context("invalid --dev-accounts value")?;
            }
            "--dev-balance" => {
                i += 1;
                dev_balance = args[i].parse().context("invalid --dev-balance value")?;
            }
            "--genesis-delay-ms" => {
                i += 1;
                genesis_delay_ms = args[i]
                    .parse()
                    .context("invalid --genesis-delay-ms value")?;
            }
            "--help" | "-h" => {
                eprintln!("Usage: genesis-ceremony [OPTIONS]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  -n, --validators <N>    Number of validators (default: 4)");
                eprintln!("  -o, --output-dir <DIR>  Output directory (default: .)");
                eprintln!("  --stake <AMOUNT>        Stake per validator (default: 1000000)");
                eprintln!("  --network <NAME>        Network preset: devnet|testnet|mainnet (default: devnet)");
                eprintln!(
                    "  --dev-accounts <N>      Fund N deterministic devnet accounts (default: 256)"
                );
                eprintln!("  --dev-balance <AMOUNT>  Balance per devnet account (default: 1e15)");
                eprintln!(
                    "  --genesis-delay-ms <MS> Slot 0 starts this long after now (default: 3000)"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("Unknown argument: {other}");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("failed to create output dir: {}", output_dir.display()))?;

    let chain_config = match network.as_str() {
        "mainnet" => ChainConfig::mainnet(),
        "testnet" => ChainConfig::testnet(),
        _ => ChainConfig::devnet(),
    };

    println!("Generating {num_validators} validator keypairs...");
    let keypairs: Vec<ValidatorKeypair> = (0..num_validators)
        .map(|_| ValidatorKeypair::generate())
        .collect();

    // Save individual key files
    for (i, kp) in keypairs.iter().enumerate() {
        let key_path = output_dir.join(format!("validator-{}.key", i + 1));
        kp.save_to_file(&key_path)
            .with_context(|| format!("failed to save key: {}", key_path.display()))?;
        println!(
            "  validator-{}: address={:?}, key={}",
            i + 1,
            kp.address(),
            key_path.display()
        );
    }

    // Build genesis config
    let mut genesis = GenesisConfig::from_keypairs(chain_config, &keypairs, stake_per_validator);
    for index in 0..dev_accounts {
        let kp = Keypair::devnet(index);
        genesis.accounts.push(GenesisAccount {
            address: Address::from(kp.to_address()),
            balance: dev_balance,
        });
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    genesis.genesis_time_ms = now_ms + genesis_delay_ms;
    genesis.timestamp = genesis.genesis_time_ms / 1000;
    genesis
        .validate()
        .context("genesis config validation failed")?;

    let genesis_path = output_dir.join("genesis.json");
    let genesis_json =
        serde_json::to_string_pretty(&genesis).context("failed to serialize genesis config")?;
    std::fs::write(&genesis_path, &genesis_json)
        .with_context(|| format!("failed to write {}", genesis_path.display()))?;

    println!("\nGenesis written to: {}", genesis_path.display());
    println!(
        "  validators: {}, stake/each: {}, total_stake: {}",
        num_validators,
        stake_per_validator,
        stake_per_validator * num_validators as u128
    );
    println!(
        "  chain: {} ({})",
        genesis.chain_config.chain.chain_id, network
    );

    // Print env var hints for docker-compose
    println!("\nDocker environment variables:");
    println!("  AETHER_GENESIS_PATH={}", genesis_path.display());
    for (i, _) in keypairs.iter().enumerate() {
        let key_path = output_dir.join(format!("validator-{}.key", i + 1));
        println!(
            "  AETHER_VALIDATOR_KEY={} (validator-{})",
            key_path.display(),
            i + 1
        );
    }

    Ok(())
}
