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

use futures::stream::{self, StreamExt};
use std::time::Instant;
use tracing::{debug, info, trace};

use crate::engine::bittorrent::download::command::BtDownloadCommand;
use crate::engine::bittorrent::download::execute::piece_download::session::peer_dials::PeerDialConfig;
use crate::engine::bittorrent::peer::connection::BtPeerConn;
use crate::engine::bittorrent::peer::interaction::{BtPeerInteraction, PEER_CONNECT_SETTLE_TIME};
use crate::request::request_group::BtPeerSource;
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
}

/// Establish one bounded batch of discovered peer handshakes.
///
/// The piece-session dial queue runs this work independently from its
/// coordinator loop, then transfers ready sockets back for Swarm actor
/// admission. Failures advance the candidate stream; the candidate tail is
/// not limited by the number of simultaneous dials.
pub(in crate::engine::bittorrent::download::execute) async fn connect_discovered_peers(
    new_peers: Vec<PeerAddr>,
    source: BtPeerSource,
    max_connections: usize,
    config: PeerDialConfig,
) -> Vec<BtPeerConn> {
    // This is the outbound connection path for PEX, tracker, and DHT peers.
    // PEX-specific gating happens before this function is called; tracker
    // and DHT peers remain connectable when PEX is disabled.
    if new_peers.is_empty() || max_connections == 0 {
        return Vec::new();
    }

    info!(
        source = ?source,
        count = new_peers.len(),
        "[BT] Attempting to connect to newly discovered peers"
    );

    // Connect concurrently and start using the first ready peers without
    // waiting for every stale or unresponsive address to time out.
    // The candidate list is not truncated to this concurrency bound: if
    // early endpoints fail, later candidates still enter the dial stream.
    let dial_concurrency = config.max_concurrent_dials.min(new_peers.len()).max(1);
    let mut results = stream::iter(new_peers)
        .map(|peer| {
            let info_hash = config.info_hash;
            let connection_options = config.connection_options.clone();
            let num_pieces = config.num_pieces;
            let piece_length = config.piece_length;
            let total_size = config.total_size;
            let utp_socket = config.utp_socket.clone();
            let outbound_network_policy = std::sync::Arc::clone(&config.outbound_network_policy);
            async move {
                let result = BtPeerInteraction::connect_peer_ready(
                    &peer,
                    &info_hash,
                    &connection_options,
                    num_pieces,
                    piece_length,
                    total_size,
                    utp_socket,
                    &outbound_network_policy,
                )
                .await;
                (peer, result)
            }
        })
        .buffer_unordered(dial_concurrency);
    let mut connected = Vec::with_capacity(max_connections.min(dial_concurrency));
    let mut settle_deadline = None;
    loop {
        let next = if let Some(deadline) = settle_deadline {
            tokio::select! {
                biased;
                result = results.next() => result,
                _ = tokio::time::sleep_until(deadline) => break,
            }
        } else {
            results.next().await
        };
        let Some((peer, result)) = next else {
            break;
        };
        match result {
            Ok(mut conn) => {
                debug!("[PEX] Connected to {}:{}", peer.ip, peer.port);
                conn.set_source(source);
                connected.push(conn);
                if connected.len() >= max_connections {
                    break;
                }
                settle_deadline
                    .get_or_insert_with(|| tokio::time::Instant::now() + PEER_CONNECT_SETTLE_TIME);
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

/// Send periodic BEP 11 messages through each peer's bounded actor mailbox.
pub(super) async fn send_periodic_pex_to_swarm(
    cmd: &BtDownloadCommand,
    swarm: &mut crate::engine::bittorrent::peer::message_handler::PeerSwarm,
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
                crate::engine::bittorrent::peer::message_handler::PeerCommand::SendPex(wire_bytes),
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

#[cfg(test)]
mod tests {
    use super::{BtPeerSource, connect_discovered_peers};
    use crate::engine::bittorrent::download::command::BtDownloadCommand;
    use crate::engine::bittorrent::download::execute::piece_download::session::peer_dials::PeerDialConfig;
    use crate::engine::bittorrent::peer::interaction::PEER_CONNECT_SETTLE_TIME;
    use crate::request::request_group::{DownloadOptions, GroupId};
    use aria2_protocol::bittorrent::peer::connection::PeerAddr;
    use std::time::Duration;

    #[tokio::test]
    async fn dynamically_discovered_ready_peer_is_returned_before_slow_peer_timeout() {
        let torrent = crate::engine::bittorrent::download::command_tests::build_test_torrent();
        let metadata = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
            .expect("test torrent should parse");
        let info_hash = metadata.info_hash.bytes;
        let command = BtDownloadCommand::new(
            GroupId::new(912),
            &torrent,
            &DownloadOptions {
                enable_utp: false,
                peer_connection_timeout: 2,
                ..DownloadOptions::default()
            },
            None,
        )
        .expect("test command should construct");

        let good_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let good_addr = good_listener.local_addr().unwrap();
        let good_peer = tokio::spawn(async move {
            loop {
                let (stream, _) = good_listener.accept().await.unwrap();
                if let Ok(incoming) =
                    aria2_protocol::bittorrent::peer::incoming::receive(stream, &[info_hash]).await
                {
                    if let Ok(connection) = incoming.complete([8; 20], None, false).await {
                        return connection;
                    }
                }
            }
        });

        let delayed_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let delayed_addr = delayed_listener.local_addr().unwrap();
        let delayed_peer = tokio::spawn(async move {
            loop {
                let (stream, _) = delayed_listener.accept().await.unwrap();
                if let Ok(incoming) =
                    aria2_protocol::bittorrent::peer::incoming::receive(stream, &[info_hash]).await
                {
                    if let Ok(connection) = incoming.complete([10; 20], None, false).await {
                        return connection;
                    }
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        });

        let slow_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let slow_addr = slow_listener.local_addr().unwrap();
        let slow_peer = tokio::spawn(async move {
            while let Ok((stream, _)) = slow_listener.accept().await {
                tokio::spawn(async move {
                    let _stream = stream;
                    tokio::time::sleep(Duration::from_secs(5)).await;
                });
            }
        });

        let peers = [
            PeerAddr::new(&good_addr.ip().to_string(), good_addr.port()),
            PeerAddr::new(&delayed_addr.ip().to_string(), delayed_addr.port()),
            PeerAddr::new(&slow_addr.ip().to_string(), slow_addr.port()),
        ];
        let config = PeerDialConfig::new(
            &command,
            info_hash,
            metadata.info_hash_v2,
            u32::try_from(metadata.num_pieces()).unwrap(),
            metadata.info.piece_length,
            metadata.info.length.unwrap_or_default(),
        );
        let connected = tokio::time::timeout(
            PEER_CONNECT_SETTLE_TIME + Duration::from_millis(250),
            connect_discovered_peers(peers.to_vec(), BtPeerSource::Dht, 2, config),
        )
        .await
        .expect("slow discovered peer must not block actor admission");

        assert_eq!(connected.len(), 2);
        assert!(
            connected
                .iter()
                .all(|connection| connection.source() == BtPeerSource::Dht)
        );
        drop(connected);
        drop(good_peer.await.unwrap());
        drop(delayed_peer.await.unwrap());
        slow_peer.abort();
    }
}
