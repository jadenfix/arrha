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
    Address, AggregatedVote, Block, ChainConfig, PublicKey, Signature, SlashEvidence,
    SlashEvidenceType, SlashVote, Transaction, ValidatorInfo, Vote, H256,
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
        Self::new_with_stakes(vec![stake_per_validator; num_validators])
    }

    fn new_with_stakes(stakes: Vec<u128>) -> Self {
        let num_validators = stakes.len();
        let chain_config = Arc::new(ChainConfig::devnet());
        let keypairs: Vec<ValidatorKeypair> = (0..num_validators)
            .map(|_| ValidatorKeypair::generate())
            .collect();

        let validator_infos: Vec<ValidatorInfo> = keypairs
            .iter()
            .enumerate()
            .map(|(i, kp)| validator_info_from_keypair(kp, stakes[i]))
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
            keypair
                .save_to_file(&temp_dir.path().join("validator-key.json"))
                .expect("save E2E validator key for BLS vote journey");
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

            for (validator_idx, addr) in validator_addrs.iter().enumerate() {
                let _ = node.seed_account(addr, stakes[validator_idx]);
            }

            // Register each validator in the staking state so slashing has
            // a bond to deduct from.
            for (i, addr) in validator_addrs.iter().enumerate() {
                let _ = node.staking_state_mut().register_validator(
                    *addr,
                    *addr,
                    stakes[i],
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
// Test 1: Vote observation does not mutate canonical economic stake
// ============================================================================

#[test]
fn test_observed_double_vote_does_not_mutate_economic_stake() {
    let initial_stake = 1_000_000_000u128;
    let mut network = ByzantineTestNetwork::new(4, initial_stake);

    // Run a few slots so the network is operational
    network.run_slots(5);

    // Validator 0 is Byzantine: sends conflicting votes at slot 1
    let byzantine_addr = network.validator_addrs[0];
    network.inject_conflicting_votes(0, 1);

    // Conflicting local observations are not canonical block evidence. They
    // must not mutate persisted staking economics outside block execution.
    for (i, node) in network.nodes.iter().enumerate() {
        let validator = node.staking_state().get_validator(&byzantine_addr);

        if let Some(v) = validator {
            assert_eq!(
                v.staked_amount, initial_stake,
                "Node {} must defer economic slashing until evidence is in a block",
                i
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
// Test 4: Repeated local observations do not alter economic stake
// ============================================================================

#[test]
fn test_no_double_slash_for_same_offense() {
    let initial_stake = 1_000_000_000u128;
    let mut network = ByzantineTestNetwork::new(4, initial_stake);

    network.run_slots(5);

    let byzantine_addr = network.validator_addrs[0];

    // Inject conflicting votes at slot 1
    network.inject_conflicting_votes(0, 1);

    // Record stake after the first local observation.
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

    // Inject the SAME conflicting votes again — economic stake must remain unchanged.
    network.inject_conflicting_votes(0, 1);

    for (i, node) in network.nodes.iter().enumerate() {
        let stake_after_second = node
            .staking_state()
            .get_validator(&byzantine_addr)
            .map(|v| v.staked_amount)
            .unwrap_or(initial_stake);

        assert_eq!(
            stake_after_second, stake_after_first[i],
            "Node {} must not apply an uncommitted offense to economic stake",
            i
        );
    }
}

// ============================================================================
// Test 5: Multiple local observations leave economic stake unchanged
// ============================================================================

#[test]
fn test_multiple_byzantine_validators_independently_slashed() {
    let initial_stake = 1_000_000_000u128;
    let mut network = ByzantineTestNetwork::new(4, initial_stake);

    network.run_slots(5);

    // Validators 0 and 1 both double-vote at different slots
    network.inject_conflicting_votes(0, 1);
    network.inject_conflicting_votes(1, 2);

    for (i, node) in network.nodes.iter().enumerate() {
        // Validators 0 and 1 have only been observed locally; block evidence
        // must drive deterministic economic slashing on every node.
        if let Some(v0) = node
            .staking_state()
            .get_validator(&network.validator_addrs[0])
        {
            assert_eq!(
                v0.staked_amount, initial_stake,
                "Node {}: validator 0 stake must wait for block evidence",
                i
            );
        }

        // Validator 1 should be slashed
        if let Some(v1) = node
            .staking_state()
            .get_validator(&network.validator_addrs[1])
        {
            assert_eq!(
                v1.staked_amount, initial_stake,
                "Node {}: validator 1 stake must wait for block evidence",
                i
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

#[test]
fn test_slashed_validator_votes_and_qc_keep_epoch_snapshot_stake() {
    // The first two validators sum to exactly 2/3 of the epoch stake. A 5%
    // mid-epoch slash of validator 0 makes that same valid QC appear below
    // quorum if verification switches to live stake.
    let initial_stakes = vec![2_000, 1_000, 750, 750];
    let mut network = ByzantineTestNetwork::new_with_stakes(initial_stakes.clone());
    network.run_slots(20);

    let finalized = network.nodes[1].finalized_slot();
    let (parent, mut child) = (2..20)
        .rev()
        .find_map(|slot| {
            let child = network.nodes[1].get_block_by_slot(slot)?;
            if child.header.slot <= finalized {
                return None;
            }
            let parent = network.nodes[1].get_block_by_hash(child.header.parent_hash)?;
            Some((parent, child))
        })
        .expect("network should retain an unfinalized parent-child block pair");

    let offense_slot = network.nodes[0].current_slot();
    network.inject_conflicting_votes(0, offense_slot);

    // Build a cryptographically valid QC for the existing parent from the
    // exact frozen-stake quorum (validator 0 plus validator 1). Changing only
    // the child's timestamp gives it a fresh block hash while keeping its
    // empty-transaction state transition identical.
    let keys: Vec<ValidatorKeypair> = network
        ._temp_dirs
        .iter()
        .map(|dir| {
            ValidatorKeypair::load_from_file(&dir.path().join("validator-key.json"))
                .expect("reload E2E key for valid QC")
        })
        .collect();
    let mut vote_message = Vec::with_capacity(40);
    vote_message.extend_from_slice(parent.hash().as_bytes());
    vote_message.extend_from_slice(&parent.header.slot.to_le_bytes());
    let signatures = vec![
        keys[0].bls.sign(&vote_message),
        keys[1].bls.sign(&vote_message),
    ];
    child.aggregated_vote = Some(AggregatedVote {
        slot: parent.header.slot,
        block_hash: parent.hash(),
        aggregated_signature: aether_crypto_bls::aggregate_signatures(&signatures).unwrap(),
        signers: vec![keys[0].public_key(), keys[1].public_key()],
        total_stake: initial_stakes[0] + initial_stakes[1],
    });
    child.header.timestamp = child.header.timestamp.saturating_add(1);

    network.nodes[1]
        .on_block_received(child)
        .expect("peer must verify QC against epoch-frozen validator weights");

    // Every node should emit the slashed validator's next vote using its
    // epoch-frozen stake, and peers must accept that vote.
    let mut post_slash_vote = None;
    for _ in 0..8 {
        network.run_slots(1);
        if let Some(index) = network.pending_votes.iter().position(|vote| {
            vote.validator.to_address() == network.validator_addrs[0] && vote.slot > offense_slot
        }) {
            post_slash_vote = Some(network.pending_votes.remove(index));
            break;
        }
    }
    let post_slash_vote = post_slash_vote.expect("slashed validator should vote on a later block");
    assert_eq!(
        post_slash_vote.stake, initial_stakes[0],
        "outbound vote must use epoch-frozen stake"
    );
    network.nodes[1]
        .on_vote_received(post_slash_vote)
        .expect("peer should accept a vote with epoch-frozen stake");
}

fn signed_double_sign_evidence(
    signer: &ValidatorKeypair,
    reported_validator: Address,
    slot: u64,
) -> SlashEvidence {
    let validator_pubkey = PublicKey::from_bytes(signer.bls.public_key());
    let hash_a = H256::from_slice(&[0xA1; 32]).unwrap();
    let hash_b = H256::from_slice(&[0xB2; 32]).unwrap();
    let sign = |block_hash: H256| {
        let mut message = Vec::with_capacity(40);
        message.extend_from_slice(block_hash.as_bytes());
        message.extend_from_slice(&slot.to_le_bytes());
        Signature::from_bytes(signer.bls.sign(&message))
    };
    SlashEvidence {
        validator: reported_validator,
        slash_rate_bps: 0,
        reason: "double_sign".to_string(),
        vote1: Some(SlashVote {
            slot,
            block_hash: hash_a,
            validator: reported_validator,
            validator_pubkey: validator_pubkey.clone(),
            signature: sign(hash_a),
        }),
        vote2: Some(SlashVote {
            slot,
            block_hash: hash_b,
            validator: reported_validator,
            validator_pubkey,
            signature: sign(hash_b),
        }),
        evidence_type: Some(SlashEvidenceType::DoubleSign),
    }
}

fn signed_surround_claim(signer: &ValidatorKeypair, validator: Address) -> SlashEvidence {
    let validator_pubkey = PublicKey::from_bytes(signer.bls.public_key());
    let signed_vote = |slot: u64, byte: u8| {
        let block_hash = H256::from_slice(&[byte; 32]).unwrap();
        let mut message = Vec::with_capacity(40);
        message.extend_from_slice(block_hash.as_bytes());
        message.extend_from_slice(&slot.to_le_bytes());
        SlashVote {
            slot,
            block_hash,
            validator,
            validator_pubkey: validator_pubkey.clone(),
            signature: Signature::from_bytes(signer.bls.sign(&message)),
        }
    };
    SlashEvidence {
        validator,
        slash_rate_bps: 0,
        reason: "surround_vote".to_string(),
        vote1: Some(signed_vote(60, 0x61)),
        vote2: Some(signed_vote(61, 0x62)),
        evidence_type: Some(SlashEvidenceType::SurroundVote),
    }
}

fn submit_signed_double_vote_pair(
    node: &mut Node,
    signer: &ValidatorKeypair,
    reported_validator: Address,
    stake: u128,
    slot: u64,
) {
    let evidence = signed_double_sign_evidence(signer, reported_validator, slot);
    for vote in [evidence.vote1.unwrap(), evidence.vote2.unwrap()] {
        let network_vote = Vote {
            slot: vote.slot,
            block_hash: vote.block_hash,
            validator: signer.public_key(),
            signature: vote.signature,
            stake,
        };
        // Votes can be locally detected before their referenced block is
        // known. The detector must still retain the verified proof for a
        // later locally produced block.
        let _ = node.on_vote_received(network_vote);
    }
}

fn restart_slash_test_node(
    db_path: &std::path::Path,
    validator_key_path: &std::path::Path,
    victim_key: &ValidatorKeypair,
    stake: u128,
) -> (Node, ValidatorKeypair) {
    let key = ValidatorKeypair::load_from_file(validator_key_path).unwrap();
    let validator = key.address();
    let infos = vec![validator_info_from_keypair(&key, stake)];
    let vrf_keys = vec![(validator, *key.vrf.public_key())];
    let bls_keys = vec![
        (
            validator,
            key.bls.public_key(),
            key.bls.proof_of_possession(),
        ),
        (
            victim_key.address(),
            victim_key.bls.public_key(),
            victim_key.bls.proof_of_possession(),
        ),
    ];
    let consensus = Box::new(
        create_hybrid_consensus_with_all_keys(infos, vrf_keys, bls_keys, Some(&key), 0.8, 100)
            .unwrap(),
    );
    let node = Node::new(
        db_path,
        consensus,
        Some(key.ed25519),
        Some(key.bls),
        Arc::new(ChainConfig::devnet()),
    )
    .unwrap();
    let signer = ValidatorKeypair::load_from_file(validator_key_path).unwrap();
    (node, signer)
}

#[test]
fn test_finalized_slash_uses_registered_bls_key_and_survives_restart_dedup() {
    let initial_stake = 1_000_000_000u128;
    let mut network = ByzantineTestNetwork::new(1, initial_stake);
    let validator = network.validator_addrs[0];
    let db_path = network._temp_dirs[0].path().to_path_buf();
    let key_path = db_path.join("validator-key.json");
    let victim_key = ValidatorKeypair::generate();
    let victim = victim_key.address();
    drop(network.nodes.remove(0));
    // Keep one active leader for a deterministic single-node block journey,
    // while registering a second validator BLS key for the spoofing negative
    // control.
    let (mut node, signer) =
        restart_slash_test_node(&db_path, &key_path, &victim_key, initial_stake);
    node.staking_state_mut()
        .register_validator(victim, victim, initial_stake, 0, victim)
        .unwrap();

    // The active validator signs conflicting votes with its own registered
    // BLS key while naming a different registered validator. A containing
    // block must not slash the victim.
    let spoof = signed_double_sign_evidence(&signer, victim, 50);
    let unproven_surround = signed_surround_claim(&signer, validator);
    node.propose_block_with_slash_evidence(1, vec![spoof, unproven_surround])
        .unwrap();
    for slot in 2..=4 {
        node.propose_block_with_slash_evidence(slot, Vec::new())
            .unwrap();
    }
    assert_eq!(
        node.staking_state()
            .get_validator(&victim)
            .unwrap()
            .staked_amount,
        initial_stake,
        "a validator's BLS key must not slash another registered validator"
    );

    // A correctly registered validator's signed proof is applied by its
    // block, then remains applied when the same offense is included after a
    // process restart.
    submit_signed_double_vote_pair(&mut node, &signer, validator, initial_stake, 51);
    node.shutdown().unwrap();
    drop(node);
    let (mut node, _signer) =
        restart_slash_test_node(&db_path, &key_path, &victim_key, initial_stake);
    for _ in 0..64 {
        node.tick().unwrap();
        if node
            .staking_state()
            .get_validator(&validator)
            .unwrap()
            .staked_amount
            == initial_stake - initial_stake * 500 / 10_000
        {
            break;
        }
    }
    let expected = initial_stake - initial_stake * 500 / 10_000;
    assert_eq!(
        node.staking_state()
            .get_validator(&validator)
            .unwrap()
            .staked_amount,
        expected,
        "finalized proof should reduce registered validator stake once"
    );
    for _ in 0..6 {
        node.tick().unwrap();
    }
    assert!(
        (1..=node.finalized_slot()).any(|slot| {
            node.get_block_by_slot(slot).is_some_and(|block| {
                block.slash_evidence.iter().any(|evidence| {
                    evidence.validator == validator
                        && evidence.vote1.as_ref().is_some_and(|vote| vote.slot == 51)
                })
            })
        }),
        "a locally detected valid proof must be included in a produced finalized block"
    );

    // Replay the chain through a fresh node, but remove the proof from its
    // finalized carrier while keeping the carrier header unchanged. The next
    // block's QC still certifies the original carrier ID, so it must fail
    // against the evidence-tampered parent instead of finalizing a divergent
    // staking branch under the same certified ID.
    let (_slash_slot, original_parent, mut child) = (1..=node.finalized_slot())
        .find_map(|slot| {
            let parent = node.get_block_by_slot(slot)?;
            if !parent
                .slash_evidence
                .iter()
                .any(|evidence| evidence.vote1.as_ref().is_some_and(|vote| vote.slot == 51))
            {
                return None;
            }
            let parent_hash = parent.hash();
            let child = (slot.saturating_add(1)..=node.current_slot())
                .filter_map(|child_slot| node.get_block_by_slot(child_slot))
                .find(|child| child.header.parent_hash == parent_hash)?;
            Some((slot, parent, child))
        })
        .expect("finalized slash block should have a certified child");
    let mut tampered_parent = original_parent.clone();
    tampered_parent.slash_evidence.clear();
    let tampered_parent_hash = tampered_parent.hash();
    assert_ne!(
        tampered_parent_hash,
        original_parent.hash(),
        "removing state-changing evidence must change the block ID"
    );
    let qc = child
        .aggregated_vote
        .as_ref()
        .expect("non-genesis child must carry a QC for its parent");
    assert_eq!(qc.block_hash, original_parent.hash());
    child.header.parent_hash = tampered_parent_hash;

    let replay_dir = TempDir::new().expect("create fresh replay database");
    let infos = vec![validator_info_from_keypair(&signer, initial_stake)];
    let vrf_keys = vec![(validator, *signer.vrf.public_key())];
    let bls_keys = vec![
        (
            validator,
            signer.bls.public_key(),
            signer.bls.proof_of_possession(),
        ),
        (
            victim_key.address(),
            victim_key.bls.public_key(),
            victim_key.bls.proof_of_possession(),
        ),
    ];
    let consensus = Box::new(
        create_hybrid_consensus_with_all_keys(infos, vrf_keys, bls_keys, None, 0.8, 100).unwrap(),
    );
    let mut replay = Node::new(
        replay_dir.path(),
        consensus,
        None,
        None,
        Arc::new(ChainConfig::devnet()),
    )
    .unwrap();
    replay
        .seed_account(&validator, initial_stake)
        .expect("replay node must match the funded validator genesis account");
    let mut ancestors = Vec::new();
    let mut ancestor_hash = original_parent.header.parent_hash;
    while ancestor_hash != H256::zero() {
        let ancestor = node
            .get_block_by_hash(ancestor_hash)
            .expect("canonical parent chain must retain every ancestor");
        ancestor_hash = ancestor.header.parent_hash;
        ancestors.push(ancestor);
    }
    ancestors.reverse();
    assert_eq!(
        ancestors.last().map(Block::hash).unwrap_or_else(H256::zero),
        original_parent.header.parent_hash
    );
    for ancestor in ancestors {
        let slot = ancestor.header.slot;
        while replay.current_slot().saturating_add(1) < slot {
            replay.tick().unwrap();
        }
        replay
            .on_block_received(ancestor)
            .unwrap_or_else(|err| panic!("fresh node rejected ancestor at slot {slot}: {err}"));
    }
    while replay.current_slot().saturating_add(1) < original_parent.header.slot {
        replay.tick().unwrap();
    }
    replay
        .on_block_received(tampered_parent)
        .expect("fresh node should accept the valid header with tampered evidence");
    let finalized_before_bad_qc = replay.finalized_slot();
    while replay.current_slot().saturating_add(1) < child.header.slot {
        replay.tick().unwrap();
    }
    let error = replay
        .on_block_received(child.clone())
        .expect_err("QC for the original evidence block must not certify altered evidence");
    assert!(
        error
            .to_string()
            .contains("aggregated vote references block"),
        "tampered evidence should invalidate the parent QC, got: {error}"
    );
    assert_eq!(
        replay.finalized_slot(),
        finalized_before_bad_qc,
        "a QC with mismatched evidence commitment must not advance finality"
    );
    replay.shutdown().unwrap();
    drop(replay);
    drop(node);

    let (mut restarted, restarted_signer) =
        restart_slash_test_node(&db_path, &key_path, &victim_key, initial_stake);
    assert_eq!(
        restarted
            .staking_state()
            .get_validator(&validator)
            .unwrap()
            .staked_amount,
        expected,
        "restart must load finalized slash economics"
    );
    let duplicate_proof = signed_double_sign_evidence(&restarted_signer, validator, 51);
    restarted
        .propose_block_with_slash_evidence(9, vec![duplicate_proof])
        .unwrap();
    for slot in 10..=12 {
        restarted
            .propose_block_with_slash_evidence(slot, Vec::new())
            .unwrap();
    }
    assert_eq!(
        restarted
            .staking_state()
            .get_validator(&validator)
            .unwrap()
            .staked_amount,
        expected,
        "persisted offense dedup must prevent a second slash after restart"
    );
}
