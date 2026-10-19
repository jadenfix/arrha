use std::collections::{HashMap, HashSet, VecDeque};

use libp2p::PeerId;
use sha2::{Digest, Sha256};

use crate::mesh::Mesh;
use crate::scoring::PeerScores;

/// Maximum number of message IDs retained for deduplication.
/// At ~32 bytes per entry, 100k entries ≈ 3.2 MB.
const MAX_SEEN_CACHE: usize = 100_000;

/// Maximum number of delivered message payloads retained per topic.
const MAX_DELIVERED_PER_TOPIC: usize = 10_000;

#[derive(Default)]
pub struct GossipRouter {
    mesh: Mesh,
    scores: PeerScores,
    seen: HashSet<[u8; 32]>,
    seen_order: VecDeque<[u8; 32]>,
    delivered: HashMap<String, VecDeque<Vec<u8>>>,
}

pub struct GossipOutcome {
    pub delivered: bool,
    pub forwarded_to: Vec<PeerId>,
}

impl GossipRouter {
    pub fn new() -> Self {
        GossipRouter::default()
    }

    pub fn mesh_mut(&mut self) -> &mut Mesh {
        &mut self.mesh
    }

    pub fn delivered_messages(&self, topic: &str) -> Vec<&Vec<u8>> {
        self.delivered
            .get(topic)
            .map(|msgs| msgs.iter().collect())
            .unwrap_or_default()
    }

    pub fn publish(&mut self, topic: &str, data: Vec<u8>) -> GossipOutcome {
        let id = Self::message_id(topic, &data);
        if !self.insert_seen(id) {
            return GossipOutcome {
                delivered: false,
                forwarded_to: Vec::new(),
            };
        }

        let peers = self.mesh.peers(topic);
        for peer in &peers {
            self.scores.record_success(peer);
        }
        self.push_delivered(topic, data);

        GossipOutcome {
            delivered: true,
            forwarded_to: peers,
        }
    }

    pub fn receive(&mut self, from: &PeerId, topic: &str, data: Vec<u8>) -> GossipOutcome {
        let id = Self::message_id(topic, &data);
        if !self.insert_seen(id) {
            self.scores.record_failure(from);
            return GossipOutcome {
                delivered: false,
                forwarded_to: Vec::new(),
            };
        }

        self.scores.record_success(from);
        let peers: Vec<_> = self
            .mesh
            .peers(topic)
            .into_iter()
            .filter(|peer| peer != from)
            .collect();

        self.push_delivered(topic, data);

        GossipOutcome {
            delivered: true,
            forwarded_to: peers,
        }
    }

    /// Insert a message ID into the seen cache, evicting the oldest if at capacity.
    /// Returns true if the ID was new (not previously seen).
    fn insert_seen(&mut self, id: [u8; 32]) -> bool {
        if !self.seen.insert(id) {
            return false;
        }
        self.seen_order.push_back(id);
        while self.seen.len() > MAX_SEEN_CACHE {
            if let Some(old) = self.seen_order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }

    /// Push a delivered message, evicting oldest if per-topic cap is reached.
    fn push_delivered(&mut self, topic: &str, data: Vec<u8>) {
        let queue = self.delivered.entry(topic.to_string()).or_default();
        queue.push_back(data);
        while queue.len() > MAX_DELIVERED_PER_TOPIC {
            queue.pop_front();
        }
    }

    pub(crate) fn message_id(topic: &str, data: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(topic.as_bytes());
        hasher.update(data);
        hasher.finalize().into()
    }
}
