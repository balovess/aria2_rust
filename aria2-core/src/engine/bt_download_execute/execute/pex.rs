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

use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, trace};

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
    /// Build one actor-owned peer's BEP 11 payload. The download session owns
    /// the periodic deadline; unlike the legacy connection helper, this does
    /// not mutate the command-wide PEX rate-limit timestamp per peer.
    pub(super) fn build_pex_message_for_actor(
        &self,
        remote_peer_addr: &PeerAddr,
        remote_ut_pex_id: u8,
    ) -> Option<Vec<u8>> {
        if !self.peer_exchange_enabled() || self.pex_known_peers.is_empty() {
            return None;
        }
        let bencode = PexHandler::build_pex_added(
            &self.pex_known_peers,
            remote_peer_addr,
            PexHandler::DEFAULT_MAX_PEERS,
        );
        Some(serialize_extended(remote_ut_pex_id, bencode.encode()))
    }

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
    /// Completes handshakes for discovered candidates. The caller immediately
    /// transfers each result into the torrent-owned peer swarm.
    ///
    /// # Returns
    /// Handshake-complete connections awaiting actor admission.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::engine::bt_download_execute::execute) async fn connect_to_discovered_peers(
        &mut self,
        new_peers: &[PeerAddr],
        source: BtPeerSource,
        info_hash_raw: &[u8; 20],
        num_pieces: u32,
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

        let peers_to_connect: Vec<PeerAddr> = new_peers
            .iter()
            .filter(|peer| !self.is_peer_temporarily_rejected(&peer.ip))
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

/// Send periodic BEP 11 messages through each peer's bounded actor mailbox.
pub(super) async fn send_periodic_pex_to_swarm(
    cmd: &BtDownloadCommand,
    swarm: &mut crate::engine::bt_message_handler::PeerSwarm,
    last_pex_send: &mut Instant,
    pex_send_interval_secs: u64,
) {
    if last_pex_send.elapsed().as_secs() < pex_send_interval_secs
        || !cmd.peer_exchange_enabled()
        || cmd.pex_known_peers.is_empty()
    {
        return;
    }

    *last_pex_send = Instant::now();
    let peers = swarm
        .iter()
        .filter(|actor| !actor.dead)
        .filter_map(|actor| Some((actor.actor_id, actor.endpoint, actor.ut_pex_id?)))
        .collect::<Vec<_>>();
    let mut sent_count = 0;
    let mut disconnected = Vec::new();
    for (actor_id, endpoint, remote_ut_pex_id) in peers {
        let remote_addr = PeerAddr::new(&endpoint.ip().to_string(), endpoint.port());
        let Some(wire_bytes) = cmd.build_pex_message_for_actor(&remote_addr, remote_ut_pex_id)
        else {
            continue;
        };
        if swarm
            .send_to(
                actor_id,
                crate::engine::bt_message_handler::PeerCommand::SendPex(wire_bytes),
            )
            .await
            .is_err()
        {
            disconnected.push(actor_id);
        } else {
            sent_count += 1;
        }
    }
    for actor_id in disconnected {
        swarm.mark_dead(actor_id);
    }
    if sent_count > 0 {
        info!(
            sent_count,
            "[PEX] Sent periodic BEP 11 messages through peer actors"
        );
    }
}
