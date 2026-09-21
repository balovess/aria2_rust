//! PEX (Peer Exchange, BEP 11) production wiring.
//!
//! Implements the full send/receive cycle for ut_pex extension messages:
//! - **Outbound**: Periodically build and queue PEX Extended messages on
//!   connections that support ut_pex (after BEP 10 handshake).
//! - **Inbound**: Consume negotiated `ut_pex` messages in the peer message
//!   handler, add discovered peers to the connection pool, and attempt
//!   connections.
//!
//! Wire format (BEP 10/11):
//! ```text
//! <4-byte length><0x14><remote_ut_pex_id><bencoded dict>
//!   d
//!     5:added   <compact IPv4 peer bytes>
//!     7:added.f <flags bytes>
//!     7:added6  <compact IPv6 peer bytes>
//!     9:added6.f<flags bytes>
//!     7:dropped <compact IPv4 peer bytes>
//!     9:dropped6<compact IPv6 peer bytes>
//!   e
//! ```

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, trace, warn};

use super::super::types::PeerKey;
use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_peer_interaction::{BtPeerConnectionOptions, BtPeerInteraction};
use crate::error::Result;
use crate::request::request_group::BtPeerSource;
use crate::util::rwlock_ext::RwLockRecover;
use aria2_protocol::bittorrent::extension::pex::PexHandler;
use aria2_protocol::bittorrent::message::serializer::serialize_extended;
use aria2_protocol::bittorrent::peer::connection::PeerAddr;

// ---------------------------------------------------------------------------
// Outbound PEX: building + sending
// ---------------------------------------------------------------------------

impl BtDownloadCommand {
    /// Build a complete wire-format PEX extended message for one remote peer.
    ///
    /// Returns `None` when PEX should not be sent (private torrent, interval
    /// not elapsed, or no peers to advertise).
    ///
    /// # Arguments
    /// * `remote_peer_addr` — The remote peer's address, used to exclude it
    ///   from the "added" list per BEP 11.
    /// * `remote_ut_pex_id` — The remote peer's negotiated ext_id for
    ///   `ut_pex`; BEP 10 assigns this ID independently per connection.
    pub fn build_pex_extended_message(
        &mut self,
        remote_peer_addr: &PeerAddr,
        remote_ut_pex_id: u8,
    ) -> Option<Vec<u8>> {
        // BEP 0027 and the user-facing switch both prohibit PEX.
        if !self.peer_exchange_enabled() {
            return None;
        }

        if !self.should_send_pex() {
            return None;
        }

        if self.pex_known_peers.is_empty() {
            trace!("[PEX] No known peers to exchange");
            return None;
        }

        let pex_bencode = PexHandler::build_pex_added(
            &self.pex_known_peers,
            remote_peer_addr,
            PexHandler::DEFAULT_MAX_PEERS,
        );
        let payload = pex_bencode.encode();
        let wire_bytes = serialize_extended(remote_ut_pex_id, payload);

        self.update_pex_last_send();

        debug!(
            size = wire_bytes.len(),
            known_peers = self.pex_known_peers.len(),
            "[PEX] Built Extended message (remote ext_id={})",
            remote_ut_pex_id
        );

        Some(wire_bytes)
    }

    /// Connect to peers discovered via PEX.
    ///
    /// Filters out already-connected peers and attempts to establish
    /// connections up to a reasonable limit per batch.
    ///
    /// # Returns
    /// The successfully connected peers, ready for piece scheduling.
    #[allow(clippy::too_many_arguments)]
    pub async fn connect_to_discovered_peers(
        &mut self,
        new_peers: &[PeerAddr],
        source: BtPeerSource,
        info_hash_raw: &[u8; 20],
        num_pieces: u32,
        active_connections: &[BtPeerConn],
        piece_length: u32,
        total_size: u64,
    ) -> Vec<BtPeerConn> {
        // This is the shared connection path for PEX, tracker, DHT, and
        // incoming peers. PEX-specific gating happens before this function
        // is called; tracker and DHT peers must remain connectable when PEX is
        // disabled.
        if new_peers.is_empty() {
            return Vec::new();
        }

        let already_connected: HashSet<(String, u16)> = active_connections
            .iter()
            .map(|conn| (conn.ip_addr.clone(), conn.port))
            .collect();
        let peers_to_connect: Vec<PeerAddr> = self
            .peer_coordinator
            .select_candidates(new_peers, &already_connected)
            .into_iter()
            .filter(|peer| !self.is_peer_temporarily_rejected(&peer.ip))
            .collect();

        if peers_to_connect.is_empty() {
            debug!("[PEX] All discovered peers already connected");
            return Vec::new();
        }

        info!(
            "[PEX] Attempting to connect to {} new peers discovered via PEX",
            peers_to_connect.len()
        );
        let connection_options = {
            let group = self.group.recover();
            let mut options =
                BtPeerConnectionOptions::from_download_options(group.options(), self.local_peer_id);
            options.dht_enabled = group.options().enable_dht && !self.is_private;
            options.listen_port = (self.listen_port != 0).then_some(self.listen_port);
            options
        };

        // Attempt connections sequentially. Individual errors are logged without
        // aborting the remaining connection attempts in this batch.
        let mut connected = Vec::with_capacity(peers_to_connect.len());
        for peer in &peers_to_connect {
            if self.group.recover().is_halt_requested() {
                break;
            }
            let Some(result) = self
                .connect_peer_ready_unless_halted(
                    peer,
                    info_hash_raw,
                    &connection_options,
                    num_pieces,
                    piece_length,
                    total_size,
                )
                .await
            else {
                break;
            };
            match result {
                Ok(mut conn) => {
                    debug!("[PEX] Connected to {}:{}", peer.ip, peer.port);
                    conn.set_source(source);
                    self.apply_peer_exchange_policy(&mut conn);
                    connected.push(conn);
                }
                Err(e) => {
                    debug!(
                        "[PEX] Failed to connect to {}:{}: {}",
                        peer.ip, peer.port, e
                    );
                }
            }
        }

        // Return only connections that were established successfully.
        connected
    }

    async fn connect_peer_ready_unless_halted(
        &self,
        peer: &PeerAddr,
        info_hash_raw: &[u8; 20],
        connection_options: &BtPeerConnectionOptions,
        num_pieces: u32,
        piece_length: u32,
        total_size: u64,
    ) -> Option<Result<BtPeerConn>> {
        let lifecycle_notify = self.group.recover().lifecycle_notifier();
        let lifecycle_wait = lifecycle_notify.notified();
        tokio::pin!(lifecycle_wait);
        let outbound_network_policy = Arc::clone(&self.outbound_network_policy);
        let mut connect_future = Box::pin(BtPeerInteraction::connect_peer_ready(
            peer,
            info_hash_raw,
            connection_options,
            num_pieces,
            piece_length,
            total_size,
            self.utp_socket.clone(),
            &outbound_network_policy,
        ));

        loop {
            tokio::select! {
                result = &mut connect_future => return Some(result),
                _ = &mut lifecycle_wait => {
                    if self.group.recover().is_halt_requested() {
                        return None;
                    }
                    lifecycle_wait.set(lifecycle_notify.notified());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Periodic PEX sender — called from the download loop each iteration
// ---------------------------------------------------------------------------

/// Send periodic PEX messages to connected peers (BEP 11).
///
/// For each peer in `pex_enabled_peers` that supports ut_pex (determined
/// by the BEP 10 extension handshake), this function:
/// 1. Gets the peer's remote address from `BtPeerConn`
/// 2. Builds a PEX Extended message via `PexHandler::build_pex_added`
/// 3. Queues the serialized message into the connection's send buffer
/// 4. Flushes the send buffer
///
/// The caller is responsible for checking the interval timer before calling.
pub(super) async fn send_periodic_pex(
    cmd: &mut BtDownloadCommand,
    active_connections: &mut [BtPeerConn],
    pex_enabled_peers: &HashSet<PeerKey>,
    last_pex_send: &mut Instant,
    pex_send_interval_secs: u64,
) {
    if last_pex_send.elapsed().as_secs() < pex_send_interval_secs
        || pex_enabled_peers.is_empty()
        || cmd.pex_known_peers.is_empty()
    {
        return;
    }

    *last_pex_send = Instant::now();
    let pex_peers_count = cmd.pex_known_peers.len();
    let mut sent_count = 0usize;

    for &peer_key in pex_enabled_peers.iter() {
        if let Some(conn) = active_connections
            .iter_mut()
            .find(|conn| PeerKey::from_peer(&conn.ip_addr, conn.port) == Some(peer_key))
        {
            if !conn.is_pex_enabled() {
                continue;
            }

            // Get the remote peer's address to exclude it from the added list.
            let remote_addr = PeerAddr::new(&conn.ip_addr, conn.port);

            // BEP 10 assigns extension IDs independently on every peer. The
            // wire message must use the ID advertised by this remote peer.
            let Some(remote_ut_pex_id) = conn.peer_extension_id("ut_pex") else {
                trace!(
                    "[PEX] Skipping peer {}: ut_pex was not negotiated",
                    peer_key.address()
                );
                continue;
            };

            // Build PEX Extended message for this peer.
            if let Some(wire_bytes) = cmd.build_pex_extended_message(&remote_addr, remote_ut_pex_id)
            {
                conn.queue_message(wire_bytes);

                // Flush this peer's send buffer immediately.
                if let Err(e) = conn.flush_send_buffer().await {
                    warn!(
                        "[PEX] Failed to flush send buffer for peer {} ({}:{}): {}",
                        peer_key.address(),
                        conn.ip_addr,
                        conn.port,
                        e
                    );
                    continue;
                }

                sent_count += 1;
                trace!(
                    "[PEX] Sent PEX to peer {} ({}:{}) with {} known peers",
                    peer_key.address(),
                    conn.ip_addr,
                    conn.port,
                    pex_peers_count
                );
            }
        }
    }

    if sent_count > 0 {
        info!(
            "[PEX] Periodic PEX exchange: sent to {}/{} enabled peers, {} known peers",
            sent_count,
            pex_enabled_peers.len(),
            pex_peers_count
        );
    }
}
