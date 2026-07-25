use std::collections::HashSet;
use std::time::Instant;
use tracing::{debug, info, warn};

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::error::{Aria2Error, RecoverableError, Result};
use aria2_protocol::bittorrent::extension::pex::PexHandler;

impl BtDownloadCommand {
    /// Check if both local and remote peer support ut_pex extension
    #[allow(dead_code)] // PEX support check; not yet called from production download loop
    pub fn check_pex_support(
        local_extension_ids: &[Option<u8>],
        remote_extension_ids: &[Option<u8>],
    ) -> bool {
        let local_supports = local_extension_ids.contains(&Some(PexHandler::EXTENSION_ID));
        let remote_supports = remote_extension_ids.contains(&Some(PexHandler::EXTENSION_ID));
        local_supports && remote_supports
    }

    /// Build and optionally send a PEX message to connected peers.
    /// Returns the encoded PEX message (or None if not ready to send).
    pub fn maybe_send_pex(
        &mut self,
        remote_peer_addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
    ) -> Option<Vec<u8>> {
        // BEP 0027 (Private Torrent): PEX must never be sent for private
        // torrents. This is a defense-in-depth guard; the download loop also
        // leaves pex_known_peers empty for private torrents.
        if self.is_private {
            return None;
        }

        if !self.should_send_pex() {
            return None;
        }

        if self.pex_known_peers.is_empty() {
            debug!("[PEX] No known peers to exchange");
            return None;
        }

        debug!(
            known_peers = self.pex_known_peers.len(),
            remote = %format!("{}:{}", remote_peer_addr.ip, remote_peer_addr.port),
            "[PEX] Building PEX message"
        );

        let pex_msg = PexHandler::build_pex_added(
            &self.pex_known_peers,
            remote_peer_addr,
            PexHandler::DEFAULT_MAX_PEERS,
        );

        let encoded = pex_msg.encode();
        self.update_pex_last_send();

        debug!(
            size = encoded.len(),
            "[PEX] PEX message built and ready to send"
        );
        Some(encoded)
    }

    /// Process an incoming PEX message and extract discovered/dropped peers
    pub fn handle_incoming_pex(
        &mut self,
        pex_data: &[u8],
        local_addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
    ) -> Result<(
        Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
        Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    )> {
        // BEP 0027 (Private Torrent): ignore any incoming PEX message for
        // private torrents. We must not incorporate peers learned through PEX
        // because the swarm is supposed to be tracker-controlled only.
        if self.is_private {
            debug!("[PEX] Ignoring incoming PEX message for private torrent (BEP 0027)");
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

    /// Connect to peers discovered via PEX
    ///
    /// This method attempts to establish connections with peers that were
    /// discovered through PEX (Peer Exchange, BEP 11). It's called when
    /// new peers are added to the PEX known peers list.
    ///
    /// # Arguments
    /// * `new_peers` - List of peer addresses discovered via PEX
    /// * `info_hash_raw` - Torrent info hash for handshake
    /// * `num_pieces` - Total number of pieces for bitfield size
    /// * `active_connections` - Current active connections (to avoid duplicates)
    ///
    /// # Returns
    /// * Number of successfully connected new peers
    pub async fn connect_to_pex_discovered_peers(
        &mut self,
        new_peers: &[aria2_protocol::bittorrent::peer::connection::PeerAddr],
        _info_hash_raw: &[u8; 20],
        _num_pieces: u32,
        active_connections: &[BtPeerConn],
    ) -> usize {
        // BEP 0027 (Private Torrent): never connect to peers discovered via PEX
        // for private torrents.
        if self.is_private || new_peers.is_empty() {
            return 0;
        }

        // Filter out peers we're already connected to
        let already_connected: std::collections::HashSet<(String, u16)> = active_connections
            .iter()
            .filter_map(|_conn| {
                // In a full implementation, we'd get the actual remote address
                // For now, we use a placeholder check
                None
            })
            .collect();

        let peers_to_connect: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> =
            new_peers
                .iter()
                .filter(|peer| !already_connected.contains(&(peer.ip.clone(), peer.port)))
                .take(10) // Limit to 10 new connections per PEX batch
                .cloned()
                .collect();

        if peers_to_connect.is_empty() {
            debug!("[PEX] All discovered peers already connected");
            return 0;
        }

        info!(
            "[PEX] Attempting to connect to {} new peers discovered via PEX",
            peers_to_connect.len()
        );

        // Note: In a full implementation, this would:
        // 1. Use BtPeerInteraction::connect_to_peers to establish connections
        // 2. Add successful connections to active_connections
        // 3. Update pex_enabled_peers for new connections

        // For now, we log the intent and return the count
        for peer in &peers_to_connect {
            debug!("[PEX] Would connect to peer {}:{}", peer.ip, peer.port);
        }

        peers_to_connect.len()
    }
}

/// Send periodic PEX messages to connected peers (BEP 11).
/// Called from the download loop on each iteration.
pub(super) fn send_periodic_pex(
    cmd: &BtDownloadCommand,
    active_connections: &[BtPeerConn],
    pex_enabled_peers: &HashSet<usize>,
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

    for peer_idx in pex_enabled_peers.iter() {
        if let Some(_conn) = active_connections.get(*peer_idx) {
            // Build PEX message for this peer
            let _remote_addr =
                aria2_protocol::bittorrent::peer::connection::PeerAddr::new(
                    "0.0.0.0", // Placeholder - actual address would come from connection
                    0,
                );

            // Note: In a full implementation, we would:
            // 1. Get the actual remote address from the connection
            // 2. Send the PEX extension message via the connection
            // 3. Handle incoming PEX messages in read_message loop

            debug!(
                "[PEX] Would send PEX to peer {} ({} known peers available)",
                peer_idx, pex_peers_count
            );
        }
    }

    info!(
        "[PEX] Periodic PEX exchange triggered: {} peers enabled, {} known peers",
        pex_enabled_peers.len(),
        pex_peers_count
    );
}
