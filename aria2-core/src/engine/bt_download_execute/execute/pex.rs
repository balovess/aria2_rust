//! PEX (Peer Exchange, BEP 11) production wiring.
//!
//! Implements the full send/receive cycle for ut_pex extension messages:
//! - **Outbound**: Periodically build and queue PEX Extended messages on
//!   connections that support ut_pex (after BEP 10 handshake).
//! - **Inbound**: Process `ExtensionUpdate::PeerExchange` from the dispatch
//!   layer, add discovered peers to the known list, and attempt connections.
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
use std::time::Instant;
use tracing::{debug, info, trace, warn};

use super::super::types::PeerKey;
use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_peer_interaction::BtPeerInteraction;
use crate::engine::extension_registry::ExtensionUpdate;
use crate::error::{Aria2Error, RecoverableError, Result};
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
        // BEP 0027 (Private Torrent): PEX must never be sent.
        if self.is_private {
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

    /// Process an incoming PEX message and extract discovered/dropped peers.
    pub fn handle_incoming_pex(
        &mut self,
        pex_data: &[u8],
        local_addr: &PeerAddr,
    ) -> Result<(Vec<PeerAddr>, Vec<PeerAddr>)> {
        // BEP 0027 (Private Torrent): ignore any incoming PEX message.
        if self.is_private {
            debug!("[PEX] Ignoring incoming PEX for private torrent (BEP 0027)");
            return Ok((Vec::new(), Vec::new()));
        }

        match PexHandler::process_received_pex(pex_data, local_addr) {
            Ok((added, dropped)) => {
                if !added.is_empty() {
                    info!(count = added.len(), "[PEX] Discovered new peers from PEX");
                    for peer in &added {
                        self.add_pex_peer(peer.clone());
                    }
                }
                if !dropped.is_empty() {
                    debug!(count = dropped.len(), "[PEX] Peers to drop from PEX");
                }
                Ok((added, dropped))
            }
            Err(e) => {
                warn!(error = %e, "[PEX] Failed to process incoming PEX message");
                Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: format!("PEX processing failed: {}", e),
                    },
                ))
            }
        }
    }

    /// Process an `ExtensionUpdate::PeerExchange` received from the dispatch
    /// layer, converting compact peer representations to `PeerAddr` and
    /// feeding them into the known-peers list.
    ///
    /// Returns the list of newly added peer addresses (for connecting).
    pub fn process_pex_extension_update(
        &mut self,
        update: &ExtensionUpdate,
        _local_addr: &PeerAddr,
    ) -> Vec<PeerAddr> {
        // BEP 0027: never process PEX for private torrents.
        if self.is_private {
            return Vec::new();
        }

        let (added_v4, added_v6, dropped_v4, dropped_v6) = match update {
            ExtensionUpdate::PeerExchange {
                added_v4,
                added_v6,
                dropped_v4,
                dropped_v6,
            } => (added_v4, added_v6, dropped_v4, dropped_v6),
            _ => return Vec::new(),
        };

        let mut new_peers = Vec::new();

        // Convert compact IPv4 peers → PeerAddr
        for compact in added_v4 {
            let ip = std::net::Ipv4Addr::from(*compact.ip());
            let addr = PeerAddr::new(&ip.to_string(), compact.port());
            if !self.pex_known_peers.contains(&addr) {
                self.add_pex_peer(addr.clone());
                new_peers.push(addr);
            }
        }

        // Convert compact IPv6 peers → PeerAddr
        for compact in added_v6 {
            let ip = std::net::Ipv6Addr::from(*compact.ip());
            let addr = PeerAddr::new(&ip.to_string(), compact.port());
            if !self.pex_known_peers.contains(&addr) {
                self.add_pex_peer(addr.clone());
                new_peers.push(addr);
            }
        }

        // Remove dropped peers from the known-peers list
        let mut dropped_addrs = Vec::new();
        for compact in dropped_v4 {
            let ip = std::net::Ipv4Addr::from(*compact.ip());
            let addr = PeerAddr::new(&ip.to_string(), compact.port());
            dropped_addrs.push(addr);
        }
        for compact in dropped_v6 {
            let ip = std::net::Ipv6Addr::from(*compact.ip());
            let addr = PeerAddr::new(&ip.to_string(), compact.port());
            dropped_addrs.push(addr);
        }
        if !dropped_addrs.is_empty() {
            self.pex_known_peers.retain(|p| !dropped_addrs.contains(p));
            debug!(
                dropped = dropped_addrs.len(),
                remaining = self.pex_known_peers.len(),
                "[PEX] Removed dropped peers from known list"
            );
        }

        if !new_peers.is_empty() {
            info!(
                new = new_peers.len(),
                total = self.pex_known_peers.len(),
                "[PEX] Added new peers from extension update"
            );
        }

        new_peers
    }

    /// Connect to peers discovered via PEX.
    ///
    /// Filters out already-connected peers and attempts to establish
    /// connections up to a reasonable limit per batch.
    ///
    /// # Returns
    /// The successfully connected peers, ready for piece scheduling.
    pub async fn connect_to_pex_discovered_peers(
        &mut self,
        new_peers: &[PeerAddr],
        info_hash_raw: &[u8; 20],
        num_pieces: u32,
        active_connections: &[BtPeerConn],
        piece_length: u32,
        total_size: u64,
    ) -> Vec<BtPeerConn> {
        // BEP 0027: never connect to PEX-discovered peers for private torrents.
        if self.is_private || new_peers.is_empty() {
            return Vec::new();
        }

        let already_connected: HashSet<(String, u16)> = active_connections
            .iter()
            .map(|conn| (conn.ip_addr.clone(), conn.port))
            .collect();
        let peers_to_connect: Vec<PeerAddr> = new_peers
            .iter()
            .filter(|peer| !already_connected.contains(&(peer.ip.clone(), peer.port)))
            .filter(|peer| !self.is_peer_temporarily_rejected(&peer.ip))
            .take(10) // Limit to 10 new connections per PEX batch
            .cloned()
            .collect();

        if peers_to_connect.is_empty() {
            debug!("[PEX] All discovered peers already connected");
            return Vec::new();
        }

        info!(
            "[PEX] Attempting to connect to {} new peers discovered via PEX",
            peers_to_connect.len()
        );
        let (require_crypto, force_encrypt) = {
            let group = self.group.recover();
            (
                group.options().bt_require_crypto,
                group.options().bt_force_encrypt,
            )
        };

        // Attempt connections. Errors are logged but don't fail the batch.
        let mut connected = Vec::with_capacity(peers_to_connect.len());
        for peer in &peers_to_connect {
            match BtPeerInteraction::connect_peer_ready(
                peer,
                info_hash_raw,
                require_crypto,
                force_encrypt,
                num_pieces,
                piece_length,
                total_size,
            )
            .await
            {
                Ok(conn) => {
                    debug!("[PEX] Connected to {}:{}", peer.ip, peer.port);
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

// ---------------------------------------------------------------------------
// Inbound PEX: process ExtensionUpdate from dispatch
// ---------------------------------------------------------------------------

/// Process an `ExtensionUpdate::PeerExchange` received from the interaction
/// loop's dispatch layer. Converts compact peers to `PeerAddr`, adds them
/// to the known-peers list, and returns the newly added peers for
/// potential connection.
///
/// This is the bridge between the BEP 10/11 dispatch in `BtPeerInteractive`
/// and the download command's PEX state.
///
/// # Wiring path
///
/// When `BtPeerInteractive::do_interaction_processing()` returns
/// `InteractionResult::Continue { pex_update: Some(..), .. }`, the caller
/// should invoke this function to feed the discovered peers into the
/// known-peers list. The current download loop (`download_pieces_loop`)
/// uses raw `BtMessageHandler` calls rather than the full interaction
/// loop, so this path will become active once the interaction loop is
/// wired into the command execution framework.
#[allow(dead_code)] // Will be called from interaction loop wiring (see doc above)
pub(super) fn process_incoming_pex_update(
    cmd: &mut BtDownloadCommand,
    update: &ExtensionUpdate,
    local_addr: &PeerAddr,
) -> Vec<PeerAddr> {
    cmd.process_pex_extension_update(update, local_addr)
}
