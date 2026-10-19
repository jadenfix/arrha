use std::net::SocketAddr;

use anyhow::{Context, Result};
use bytes::Bytes;
use quinn::{Connection, RecvStream, SendStream};
use tracing::debug;

/// QUIC connection wrapper with streaming API
///
/// Provides send/receive primitives for validator communication.
/// Uses unidirectional streams for one-way messages (most common)
/// and bidirectional streams for request/response patterns.
pub struct QuicConnection {
    inner: Connection,
}

impl QuicConnection {
    pub(crate) fn new(connection: Connection) -> Self {
        QuicConnection { inner: connection }
    }

    /// Get the remote address of this connection
    pub fn remote(&self) -> SocketAddr {
        self.inner.remote_address()
    }

    /// Send a message on a unidirectional stream
    ///
    /// Opens a new stream, writes the data, and closes it.
    /// This is the most efficient pattern for one-way messages
    /// like block propagation, vote broadcasts, etc.
    pub async fn send(&self, data: impl Into<Bytes>) -> Result<()> {
        let mut stream = self
            .inner
            .open_uni()
            .await
            .context("Failed to open uni stream")?;

        let data = data.into();
        stream
            .write_all(&data)
            .await
            .context("Failed to write to stream")?;

        stream.finish().context("Failed to finish stream")?;

        debug!("Sent {} bytes to {}", data.len(), self.remote());

        Ok(())
    }

    /// Send a message on a bidirectional stream and await a response
    ///
    /// Useful for RPC-style request/response patterns like
    /// repair requests, state sync queries, etc.
    pub async fn send_request(&self, data: impl Into<Bytes>) -> Result<Vec<u8>> {
        let (mut send, mut recv) = self
            .inner
            .open_bi()
            .await
            .context("Failed to open bi stream")?;

        // Send request
        let data = data.into();
        send.write_all(&data)
            .await
            .context("Failed to write request")?;
        send.finish().context("Failed to finish send")?;

        // Receive response
        let response = recv
            .read_to_end(4_000_000) // 4MB max response
            .await
            .context("Failed to read response")?;

        debug!(
            "Sent {} bytes, received {} bytes from {}",
            data.len(),
            response.len(),
            self.remote()
        );

        Ok(response)
    }

    /// Accept an incoming unidirectional stream
    pub async fn accept_uni(&self) -> Result<RecvStream> {
        self.inner
            .accept_uni()
            .await
            .context("Failed to accept uni stream")
    }

    /// Accept an incoming bidirectional stream
    pub async fn accept_bi(&self) -> Result<(SendStream, RecvStream)> {
        self.inner
            .accept_bi()
            .await
            .context("Failed to accept bi stream")
    }

    /// Read all data from a stream (up to 4MB)
    pub async fn read_stream(stream: &mut RecvStream) -> Result<Vec<u8>> {
        stream
            .read_to_end(4_000_000)
            .await
            .context("Failed to read stream")
    }

    /// Close the connection gracefully
    pub fn close(&self, reason: &str) {
        self.inner.close(0u32.into(), reason.as_bytes());
    }

    /// Get connection statistics for monitoring
    pub fn stats(&self) -> ConnectionStats {
        let stats = self.inner.stats();
        // Note: Quinn doesn't expose total bytes, only UDP datagrams
        // For now, use 0 as placeholder until we add our own tracking
        ConnectionStats {
            bytes_sent: 0,
            bytes_received: 0,
            rtt: stats.path.rtt,
        }
    }
}

/// Connection statistics for monitoring
#[derive(Debug, Clone)]
pub struct ConnectionStats {
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub rtt: std::time::Duration,
}
