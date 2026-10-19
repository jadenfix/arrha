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
