// ============================================================================
// E2E BYZANTINE NETWORK TEST
// ============================================================================
// Tests the full multi-node slashing pipeline: a Byzantine validator sends
// conflicting votes that propagate through the network, every node detects the
// double-sign, slashes the offender's stake, and the honest majority continues
// producing and finalizing blocks.
//
// This bridges the gap between:
//   - consensus-level tests (crates/consensus/tests/byzantine_fault.rs)
//   - single-node slashing tests (crates/node/src/node.rs unit tests)
// by verifying the full flow across 4 cooperating nodes.
// ============================================================================

use aether_node::{
    create_hybrid_consensus_with_all_keys, validator_info_from_keypair, Node, OutboundMessage,
    ValidatorKeypair,
};
use aether_types::{
    Address, Block, ChainConfig, PublicKey, Signature, Transaction, ValidatorInfo, Vote, H256,
};
use std::sync::Arc;
use tempfile::TempDir;

/// Network harness that retains validator public keys for crafting Byzantine votes.
struct ByzantineTestNetwork {
    nodes: Vec<Node>,
    /// Ed25519 public keys for each validator (index-aligned with nodes).
    validator_pubkeys: Vec<PublicKey>,
    /// Addresses for each validator.
    validator_addrs: Vec<Address>,
    _temp_dirs: Vec<TempDir>,
    pending_blocks: Vec<Block>,
    pending_votes: Vec<Vote>,
    pending_txs: Vec<Transaction>,
}

impl ByzantineTestNetwork {
    fn new(num_validators: usize, stake_per_validator: u128) -> Self {
        let chain_config = Arc::new(ChainConfig::devnet());
        let keypairs: Vec<ValidatorKeypair> = (0..num_validators)
            .map(|_| ValidatorKeypair::generate())
            .collect();

        let validator_infos: Vec<ValidatorInfo> = keypairs
            .iter()
            .map(|kp| validator_info_from_keypair(kp, 1_000))
            .collect();

        let vrf_pubkeys: Vec<(Address, [u8; 32])> = keypairs
            .iter()
            .map(|kp| (kp.address(), *kp.vrf.public_key()))
            .collect();

        let bls_pubkeys: Vec<(Address, Vec<u8>, Vec<u8>)> = keypairs
            .iter()
            .map(|kp| {
                (
                    kp.address(),
                    kp.bls.public_key(),
                    kp.bls.proof_of_possession(),
                )
            })
            .collect();

        let validator_addrs: Vec<Address> = keypairs.iter().map(|kp| kp.address()).collect();
        let validator_pubkeys: Vec<PublicKey> = keypairs
            .iter()
            .map(|kp| PublicKey::from_bytes(kp.ed25519.public_key()))
            .collect();

        let mut nodes = Vec::new();
        let mut temp_dirs = Vec::new();

        for keypair in keypairs {
            let temp_dir = TempDir::new().unwrap();
            let consensus = Box::new(
                create_hybrid_consensus_with_all_keys(
                    validator_infos.clone(),
                    vrf_pubkeys.clone(),
                    bls_pubkeys.clone(),
                    Some(&keypair),
                    0.8,
                    100,
                )
                .expect("create consensus"),
            );

            let mut node = Node::new(
                temp_dir.path(),
                consensus,
                Some(keypair.ed25519),
                Some(keypair.bls),
                chain_config.clone(),
            )
            .expect("create node");

            for addr in &validator_addrs {
                let _ = node.seed_account(addr, stake_per_validator);
            }

            // Register each validator in the staking state so slashing has
            // a bond to deduct from.
            for (i, addr) in validator_addrs.iter().enumerate() {
                let _ = node.staking_state_mut().register_validator(
                    *addr,
                    *addr,
                    stake_per_validator,
                    validator_infos[i].commission,
                    *addr,
                );
            }

            nodes.push(node);
            temp_dirs.push(temp_dir);
        }

        ByzantineTestNetwork {
            nodes,
            validator_pubkeys,
            validator_addrs,
            _temp_dirs: temp_dirs,
            pending_blocks: Vec::new(),
            pending_votes: Vec::new(),
            pending_txs: Vec::new(),
        }
    }

    fn tick_all(&mut self) {
        for node in &mut self.nodes {
            for vote in &self.pending_votes {
                let _ = node.on_vote_received(vote.clone());
            }
        }
        self.pending_votes.clear();

        for node in &mut self.nodes {
            for block in &self.pending_blocks {
                let _ = node.on_block_received(block.clone());
            }
        }
        self.pending_blocks.clear();

        for node in &mut self.nodes {
            for tx in &self.pending_txs {
                let _ = node.submit_transaction(tx.clone());
            }
        }
        self.pending_txs.clear();

        for node in &mut self.nodes {
            node.tick().unwrap();
        }

        for node in &mut self.nodes {
            for msg in node.drain_outbound() {
                match msg {
                    OutboundMessage::BroadcastBlock(block) => {
                        self.pending_blocks.push(block);
                    }
                    OutboundMessage::BroadcastVote(vote) => {
                        self.pending_votes.push(vote);
                    }
                    OutboundMessage::BroadcastTransaction(tx) => {
                        self.pending_txs.push(tx);
                    }
                    OutboundMessage::BroadcastTransactions(txs) => {
                        self.pending_txs.extend(txs);
                    }
                    OutboundMessage::RequestBlockRange { .. } => {}
                }
            }
        }
    }

    fn run_slots(&mut self, num_slots: usize) {
        for _ in 0..num_slots {
            self.tick_all();
        }
    }

    /// Inject a conflicting vote pair from `validator_idx` into all nodes.
    /// The Byzantine validator votes for two different blocks at the same slot.
    fn inject_conflicting_votes(&mut self, validator_idx: usize, slot: u64) {
        let pubkey = &self.validator_pubkeys[validator_idx];
        let vote_a = Vote {
            slot,
            block_hash: H256::from_slice(&[0xAA; 32]).unwrap(),
            validator: pubkey.clone(),
            signature: Signature::from_bytes(vec![0u8; 64]),
            stake: 0,
        };
        let vote_b = Vote {
            slot,
            block_hash: H256::from_slice(&[0xBB; 32]).unwrap(),
            validator: pubkey.clone(),
            signature: Signature::from_bytes(vec![0u8; 64]),
            stake: 0,
        };

        // Deliver both conflicting votes to ALL nodes
        for node in &mut self.nodes {
            let _ = node.on_vote_received(vote_a.clone());
            let _ = node.on_vote_received(vote_b.clone());
        }
    }
}

// ============================================================================
// Test 1: Byzantine double-vote detected and slashed across all nodes
// ============================================================================

#[test]
fn test_byzantine_double_vote_slashed_across_network() {
    let initial_stake = 1_000_000_000u128;
    let mut network = ByzantineTestNetwork::new(4, initial_stake);

    // Run a few slots so the network is operational
    network.run_slots(5);

    // Validator 0 is Byzantine: sends conflicting votes at slot 1
    let byzantine_addr = network.validator_addrs[0];
    network.inject_conflicting_votes(0, 1);

    // Verify every node detected the double-sign and slashed the offender
    for (i, node) in network.nodes.iter().enumerate() {
        let validator = node.staking_state().get_validator(&byzantine_addr);

        if let Some(v) = validator {
            assert!(
                v.staked_amount < initial_stake,
                "Node {} should have slashed Byzantine validator's stake: got {}",
                i,
                v.staked_amount
            );
            // 5% slash: 1_000_000_000 * 500 / 10_000 = 50_000_000
            let expected = initial_stake - (initial_stake * 500 / 10_000);
            assert_eq!(
                v.staked_amount, expected,
                "Node {} should have slashed exactly 5%: expected {}, got {}",
                i, expected, v.staked_amount
            );
        }
    }
}

// ============================================================================
// Test 2: Honest validators continue producing blocks after Byzantine event
// ============================================================================

#[test]
fn test_network_continues_after_byzantine_validator_slashed() {
    let mut network = ByzantineTestNetwork::new(4, 1_000_000_000);

    // Let the network warm up
    network.run_slots(10);

    // Inject Byzantine behavior
    network.inject_conflicting_votes(0, 5);

    // Count blocks before continued operation
    let blocks_before: usize = (0..10u64)
        .filter(|s| network.nodes[1].get_block_by_slot(*s).is_some())
        .count();

    // Continue running — honest validators (1, 2, 3) should still produce blocks
    network.run_slots(40);

    let blocks_after: usize = (0..50u64)
        .filter(|s| network.nodes[1].get_block_by_slot(*s).is_some())
        .count();

    assert!(
        blocks_after > blocks_before,
        "Network must continue producing blocks after Byzantine event: before={}, after={}",
        blocks_before,
        blocks_after
    );
}

// ============================================================================
// Test 3: State convergence maintained despite Byzantine validator
// ============================================================================

#[test]
fn test_state_convergence_with_byzantine_validator() {
    let mut network = ByzantineTestNetwork::new(4, 1_000_000_000);

    // Normal operation
    network.run_slots(10);

    // Byzantine event
    network.inject_conflicting_votes(0, 5);

    // More normal operation
    network.run_slots(40);

    // All nodes must converge on the same state root
    let state_roots: Vec<H256> = network.nodes.iter().map(|n| n.get_state_root()).collect();

    let first = state_roots[0];
    assert_ne!(first, H256::zero(), "State root should be non-zero");

    for (i, root) in state_roots.iter().enumerate().skip(1) {
        assert_eq!(
            *root, first,
            "Node {} state root diverges from Node 0 after Byzantine event",
            i
        );
    }
}

// ============================================================================
// Test 4: Double-slash prevention — same offense not slashed twice
// ============================================================================

#[test]
fn test_no_double_slash_for_same_offense() {
    let initial_stake = 1_000_000_000u128;
    let mut network = ByzantineTestNetwork::new(4, initial_stake);

    network.run_slots(5);

    let byzantine_addr = network.validator_addrs[0];

    // Inject conflicting votes at slot 1
    network.inject_conflicting_votes(0, 1);

    // Record stake after first slash
    let stake_after_first: Vec<u128> = network
        .nodes
        .iter()
        .map(|n| {
            n.staking_state()
                .get_validator(&byzantine_addr)
                .map(|v| v.staked_amount)
                .unwrap_or(initial_stake)
        })
        .collect();

    // Inject the SAME conflicting votes again — should NOT slash a second time
    network.inject_conflicting_votes(0, 1);

    for (i, node) in network.nodes.iter().enumerate() {
        let stake_after_second = node
            .staking_state()
            .get_validator(&byzantine_addr)
            .map(|v| v.staked_amount)
            .unwrap_or(initial_stake);

        assert_eq!(
            stake_after_second, stake_after_first[i],
            "Node {} must not double-slash the same (validator, slot) offense",
            i
        );
    }
}

// ============================================================================
// Test 5: Multiple Byzantine validators — each independently detected
// ============================================================================

#[test]
fn test_multiple_byzantine_validators_independently_slashed() {
    let initial_stake = 1_000_000_000u128;
    let mut network = ByzantineTestNetwork::new(4, initial_stake);

    network.run_slots(5);

    // Validators 0 and 1 both double-vote at different slots
    network.inject_conflicting_votes(0, 1);
    network.inject_conflicting_votes(1, 2);

    let expected_slashed = initial_stake - (initial_stake * 500 / 10_000);

    for (i, node) in network.nodes.iter().enumerate() {
        // Validator 0 should be slashed
        if let Some(v0) = node
            .staking_state()
            .get_validator(&network.validator_addrs[0])
        {
            assert_eq!(
                v0.staked_amount, expected_slashed,
                "Node {}: validator 0 should be slashed to {}",
                i, expected_slashed
            );
        }

        // Validator 1 should be slashed
        if let Some(v1) = node
            .staking_state()
            .get_validator(&network.validator_addrs[1])
        {
            assert_eq!(
                v1.staked_amount, expected_slashed,
                "Node {}: validator 1 should be slashed to {}",
                i, expected_slashed
            );
        }

        // Validators 2 and 3 should NOT be slashed
        if let Some(v2) = node
            .staking_state()
            .get_validator(&network.validator_addrs[2])
        {
            assert_eq!(
                v2.staked_amount, initial_stake,
                "Node {}: honest validator 2 must not be slashed",
                i
            );
        }
        if let Some(v3) = node
            .staking_state()
            .get_validator(&network.validator_addrs[3])
        {
            assert_eq!(
                v3.staked_amount, initial_stake,
                "Node {}: honest validator 3 must not be slashed",
                i
            );
        }
    }

    // Network should still converge
    network.run_slots(30);
    let roots: Vec<H256> = network.nodes.iter().map(|n| n.get_state_root()).collect();
    assert!(
        roots.iter().all(|r| *r == roots[0]),
        "All nodes must converge after multiple Byzantine events"
    );
}
