use anyhow::Result;
use tokio::sync::broadcast;

use aether_types::Block;

use crate::streaming::FirehoseStream;

#[derive(Clone, Debug)]
pub struct FirehoseEvent {
    pub block: Block,
}

pub struct FirehoseServer {
    sender: broadcast::Sender<FirehoseEvent>,
}

impl FirehoseServer {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        FirehoseServer { sender }
    }

    pub fn publish(&self, block: Block) -> Result<()> {
        self.sender
            .send(FirehoseEvent { block })
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!(e))
    }

    pub fn subscribe(&self) -> FirehoseStream {
        FirehoseStream::new(self.sender.subscribe())
    }
}
