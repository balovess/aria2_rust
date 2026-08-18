//! Keep-alive management, send buffering, PEX, and message receipt
//! bookkeeping for [`BtPeerConn`].

use std::time::Duration;

use crate::error::Result;

use super::super::types::SendBuffer;
use super::BtPeerConn;

impl BtPeerConn {
    // -----------------------------------------------------------------------
    // Keep-alive management
    // -----------------------------------------------------------------------

    /// Check whether we should send a keep-alive message.
    ///
    /// Returns `true` if the configured interval has elapsed since the last
    /// keep-alive was sent.
    pub fn should_send_keepalive(&self) -> bool {
        self.last_keepalive_sent.elapsed() >= self.keep_alive_interval
    }

    /// Check whether the peer has timed out according to the configured
    /// inactivity interval.
    pub fn is_peer_timed_out(&self) -> bool {
        self.last_message_received.elapsed() >= self.peer_timeout
    }

    /// Configure the keep-alive and peer inactivity intervals for this
    /// connection. The download command applies task options here after the
    /// transport handshake completes.
    pub(crate) fn set_timeouts(&mut self, keep_alive: Duration, peer_timeout: Duration) {
        self.keep_alive_interval = keep_alive;
        self.peer_timeout = peer_timeout;
    }

    // -----------------------------------------------------------------------
    // Send buffering
    // -----------------------------------------------------------------------

    /// Queue a serialized message into the send buffer without flushing.
    ///
    /// Call [`flush_send_buffer`](Self::flush_send_buffer) later to actually
    /// write the data to the socket. This allows batching multiple small
    /// messages into a single write.
    pub fn queue_message(&mut self, data: Vec<u8>) {
        self.send_buffer.push_bytes(data);
    }

    /// Flush all queued messages in the send buffer to the socket.
    pub async fn flush_send_buffer(&mut self) -> Result<()> {
        if self.send_buffer.is_empty() {
            return Ok(());
        }
        let data = self.send_buffer.take_pending();
        self.write_raw(&data).await?;
        self.last_keepalive_sent = std::time::Instant::now();
        Ok(())
    }

    /// Get a reference to the send buffer (for inspection).
    pub fn send_buffer(&self) -> &SendBuffer {
        &self.send_buffer
    }

    /// Get a mutable reference to the send buffer.
    pub fn send_buffer_mut(&mut self) -> &mut SendBuffer {
        &mut self.send_buffer
    }

    // -----------------------------------------------------------------------
    // PEX (BEP 11) — inbound peer accumulation
    // -----------------------------------------------------------------------

    /// Drain all accumulated PEX-discovered peers from this connection.
    ///
    /// Called by the download loop after each iteration to process peers
    /// discovered via incoming ut_pex messages during block reads.
    pub fn drain_pex_peers(
        &mut self,
    ) -> Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> {
        std::mem::take(&mut self.pending_pex_peers)
    }

    // -----------------------------------------------------------------------
    // Message receipt bookkeeping
    // -----------------------------------------------------------------------

    /// Update the last-message-received timestamp to now.
    ///
    /// Should be called whenever a message is successfully read from the
    /// peer, so that [`is_peer_timed_out`](Self::is_peer_timed_out) works
    /// correctly.
    pub fn on_message_received(&mut self) {
        self.last_message_received = std::time::Instant::now();
    }
}
