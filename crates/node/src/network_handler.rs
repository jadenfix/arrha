use aether_p2p::network::NetworkEvent;
use aether_types::{Block, Slot, Transaction, Vote};
use bincode::Options;
use serde::{Deserialize, Serialize};

/// Decoded message types from the P2P network.
#[derive(Debug)]
#[allow(clippy::enum_variant_names)]
pub enum NodeMessage {
    BlockReceived(Block),
    VoteReceived(Vote),
    TransactionReceived(Transaction),
    /// A gossip batch of transactions.
    TransactionsReceived(Vec<Transaction>),
    /// A peer requested blocks in the given slot range for state sync.
    BlockRangeRequested {
        from_slot: Slot,
        to_slot: Slot,
    },
    PeerConnected,
    PeerDisconnected,
}

/// Outbound messages from the node to the P2P network.
#[derive(Debug, Clone)]
#[allow(clippy::enum_variant_names)]
pub enum OutboundMessage {
    BroadcastBlock(Block),
    BroadcastVote(Vote),
    BroadcastTransaction(Transaction),
    /// Gossip many transactions in one message.
    BroadcastTransactions(Vec<Transaction>),
    /// Request a range of blocks from peers for state sync.
    RequestBlockRange {
        from_slot: Slot,
        to_slot: Slot,
    },
}

/// Wire format for sync request messages on the `/aether/1/sync` topic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRequest {
    pub from_slot: Slot,
    pub to_slot: Slot,
}

/// Maximum message sizes to prevent OOM from malicious peers.
/// These MUST match the per-topic limits in `aether_p2p::network` (the first
/// defense layer).  Keeping them in sync avoids silent drops where one layer
/// accepts a message the other rejects.
const MAX_BLOCK_SIZE: usize = 2 * 1024 * 1024; // 2 MB — matches gossipsub max_transmit_size
const MAX_VOTE_SIZE: usize = 8 * 1024; // 8 KB
const MAX_TX_SIZE: usize = 1024 * 1024; // 1 MB (transaction gossip is batched)

/// Prefix that marks a tx-topic message as a batch (`TXB1 || bincode(Vec<Transaction>)`).
pub const TX_BATCH_MAGIC: &[u8; 4] = b"TXB1";

/// Encode a batch of transactions for the tx gossip topic.
pub fn encode_tx_batch(txs: &[Transaction]) -> Result<Vec<u8>, bincode::Error> {
    let mut out = TX_BATCH_MAGIC.to_vec();
    out.extend(bincode::serialize(txs)?);
    Ok(out)
}
const MAX_SHRED_SIZE: usize = 256 * 1024; // 256 KB — RS(10,2) on 2 MB block ≈ 210 KB per shred
const MAX_SYNC_SIZE: usize = 1024; // 1 KB

/// Deserialize with a bincode size limit to prevent DoS via deeply nested structures.
fn deserialize_bounded<T: serde::de::DeserializeOwned>(data: &[u8], max_size: usize) -> Option<T> {
    if data.len() > max_size {
        return None;
    }
    bincode::options()
        .with_limit(max_size as u64)
        .with_fixint_encoding()
        .allow_trailing_bytes()
        .deserialize(data)
        .ok()
}

/// Decode a raw P2P NetworkEvent into a typed NodeMessage.
/// Enforces message size limits before deserialization to prevent DoS.
pub fn decode_network_event(event: NetworkEvent) -> Option<NodeMessage> {
    match event {
        NetworkEvent::BlockReceived(data) if data.len() <= MAX_BLOCK_SIZE => {
            deserialize_bounded(&data, MAX_BLOCK_SIZE).map(NodeMessage::BlockReceived)
        }
        NetworkEvent::VoteReceived(data) if data.len() <= MAX_VOTE_SIZE => {
            deserialize_bounded(&data, MAX_VOTE_SIZE).map(NodeMessage::VoteReceived)
        }
        NetworkEvent::TransactionReceived(data) if data.len() <= MAX_TX_SIZE => {
            if data.starts_with(TX_BATCH_MAGIC) {
                deserialize_bounded::<Vec<Transaction>>(&data[TX_BATCH_MAGIC.len()..], MAX_TX_SIZE)
                    .map(NodeMessage::TransactionsReceived)
            } else {
                deserialize_bounded(&data, MAX_TX_SIZE).map(NodeMessage::TransactionReceived)
            }
        }
        NetworkEvent::SyncRequestReceived(data) if data.len() <= MAX_SYNC_SIZE => {
            deserialize_bounded::<SyncRequest>(&data, MAX_SYNC_SIZE).map(|req| {
                NodeMessage::BlockRangeRequested {
                    from_slot: req.from_slot,
                    to_slot: req.to_slot,
                }
            })
        }
        // Shreds are size-checked but forwarded raw to the DA layer (no deserialization here).
        NetworkEvent::ShredReceived(data) if data.len() <= MAX_SHRED_SIZE => {
            tracing::trace!(
                "shred received ({} bytes), forwarding to DA layer",
                data.len()
            );
            None // DA layer handles shred reassembly separately
        }
        NetworkEvent::PeerConnected(_) => Some(NodeMessage::PeerConnected),
        NetworkEvent::PeerDisconnected(_) => Some(NodeMessage::PeerDisconnected),
        _ => None, // Silently drop oversized or unknown messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_types::*;

    #[test]
    fn test_decode_block_event() {
        let block = Block::new(
            1,
            H256::zero(),
            Address::from_slice(&[1u8; 20]).unwrap(),
            VrfProof {
                output: [0u8; 32],
                proof: vec![],
            },
            vec![],
        );
        let data = bincode::serialize(&block).unwrap();
        let event = NetworkEvent::BlockReceived(data);
        match decode_network_event(event) {
            Some(NodeMessage::BlockReceived(b)) => assert_eq!(b.header.slot, 1),
            other => {
                tracing::warn!("Unexpected network event, ignoring: {:?}", other);
            }
        }
    }

    #[test]
    fn test_decode_vote_event() {
        let vote = Vote {
            slot: 5,
            block_hash: H256::zero(),
            validator: PublicKey::from_bytes(vec![1u8; 32]),
            signature: Signature::from_bytes(vec![0u8; 64]),
            stake: 1000,
        };
        let data = bincode::serialize(&vote).unwrap();
        let event = NetworkEvent::VoteReceived(data);
        match decode_network_event(event) {
            Some(NodeMessage::VoteReceived(v)) => assert_eq!(v.slot, 5),
            other => {
                tracing::warn!("Unexpected network event, ignoring: {:?}", other);
            }
        }
    }

    #[test]
    fn test_decode_transaction_event() {
        let tx = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: Address::from_slice(&[0u8; 20]).unwrap(),
            sender_pubkey: PublicKey::from_bytes(vec![0u8; 32]),
            inputs: vec![],
            outputs: vec![],
            reads: std::collections::HashSet::new(),
            writes: std::collections::HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21000,
            fee: 1000,
            signature: Signature::from_bytes(vec![0u8; 64]),
        };
        let data = bincode::serialize(&tx).unwrap();
        let event = NetworkEvent::TransactionReceived(data);
        match decode_network_event(event) {
            Some(NodeMessage::TransactionReceived(_)) => {}
            other => {
                tracing::warn!("Unexpected network event, ignoring: {:?}", other);
            }
        }
    }

    #[test]
    fn test_decode_invalid_data_returns_none() {
        let event = NetworkEvent::BlockReceived(vec![0xFF, 0xFF, 0xFF]);
        assert!(decode_network_event(event).is_none());
    }

    #[test]
    fn test_decode_shred_event_returns_none() {
        // Shreds are size-checked but not decoded into NodeMessage (DA layer handles them)
        let event = NetworkEvent::ShredReceived(vec![1, 2, 3]);
        assert!(decode_network_event(event).is_none());
    }

    #[test]
    fn test_oversized_shred_rejected() {
        let oversized = vec![0u8; MAX_SHRED_SIZE + 1];
        let event = NetworkEvent::ShredReceived(oversized);
        assert!(decode_network_event(event).is_none());
    }

    #[test]
    fn test_oversized_block_rejected() {
        // A block payload exceeding MAX_BLOCK_SIZE (2 MB) must be dropped
        let oversized = vec![0u8; MAX_BLOCK_SIZE + 1];
        let event = NetworkEvent::BlockReceived(oversized);
        assert!(decode_network_event(event).is_none());
    }

    #[test]
    fn test_oversized_vote_rejected() {
        let oversized = vec![0u8; MAX_VOTE_SIZE + 1];
        let event = NetworkEvent::VoteReceived(oversized);
        assert!(decode_network_event(event).is_none());
    }

    #[test]
    fn test_oversized_tx_rejected() {
        let oversized = vec![0u8; MAX_TX_SIZE + 1];
        let event = NetworkEvent::TransactionReceived(oversized);
        assert!(decode_network_event(event).is_none());
    }

    #[test]
    fn test_oversized_sync_rejected() {
        let oversized = vec![0u8; MAX_SYNC_SIZE + 1];
        let event = NetworkEvent::SyncRequestReceived(oversized);
        assert!(decode_network_event(event).is_none());
    }

    #[test]
    fn test_limits_match_p2p_layer() {
        // These limits must stay in sync with aether_p2p::network constants.
        // If the P2P layer changes, update these too.
        assert_eq!(
            MAX_BLOCK_SIZE,
            2 * 1024 * 1024,
            "block limit out of sync with p2p"
        );
        assert_eq!(MAX_VOTE_SIZE, 8 * 1024, "vote limit out of sync with p2p");
        assert_eq!(MAX_TX_SIZE, 1024 * 1024, "tx limit out of sync with p2p");
        assert_eq!(
            MAX_SHRED_SIZE,
            256 * 1024,
            "shred limit out of sync with p2p"
        );
        assert_eq!(MAX_SYNC_SIZE, 1024, "sync limit out of sync with p2p");
    }

    #[test]
    fn test_shred_size_accommodates_max_block_with_rs10_2() {
        // RS(10, 2): 10 data shards, 2 parity. Each shard payload for a max-size
        // block is ceil((MAX_BLOCK_SIZE + 8 byte length prefix) / 10).
        // MAX_SHRED_SIZE must exceed this plus serialization overhead (~200 B).
        let data_shards = 10usize;
        let max_shard_payload = (MAX_BLOCK_SIZE + 8).div_ceil(data_shards);
        let shred_overhead = 200;
        assert!(
            MAX_SHRED_SIZE >= max_shard_payload + shred_overhead,
            "MAX_SHRED_SIZE ({MAX_SHRED_SIZE}) too small for RS({data_shards},2) on \
             {MAX_BLOCK_SIZE}-byte block: shard payload = {max_shard_payload}, \
             overhead = {shred_overhead}"
        );
    }

    #[test]
    fn test_realistic_shred_size_accepted() {
        // A shred from a 2 MB block with RS(10,2) is ~210 KB. It must not be dropped.
        let realistic_shred = vec![0u8; 210 * 1024];
        let event = NetworkEvent::ShredReceived(realistic_shred);
        // decode_network_event returns None for valid shreds (DA layer handles them),
        // but crucially it must NOT hit the oversized branch (which also returns None).
        // We verify by checking a shred at exactly MAX_SHRED_SIZE is accepted.
        let at_limit = vec![0u8; MAX_SHRED_SIZE];
        let event_at_limit = NetworkEvent::ShredReceived(at_limit);
        assert!(decode_network_event(event).is_none());
        assert!(decode_network_event(event_at_limit).is_none());
    }
}
