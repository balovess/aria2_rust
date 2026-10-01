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
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tracing::{debug, info, trace};

use crate::engine::bittorrent::download::command::BtDownloadCommand;
use crate::engine::bittorrent::download::execute::piece_download::session::peer_dials::PeerDialConfig;
use crate::engine::bittorrent::peer::connection::BtPeerConn;
use crate::engine::bittorrent::peer::interaction::{BtPeerInteraction, PEER_CONNECT_SETTLE_TIME};
use crate::request::request_group::BtPeerSource;
use aria2_protocol::bittorrent::extension::pex::PexHandler;
use aria2_protocol::bittorrent::message::serializer::serialize_extended;
use aria2_protocol::bittorrent::peer::connection::PeerAddr;

pub(crate) const PEX_SEND_INTERVAL: Duration = Duration::from_secs(60);

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
                debug!(
                    source = ?source,
                    peer_ip = %peer.ip,
                    peer_port = peer.port,
                    "Connected to discovered peer"
                );
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
                    source = ?source,
                    peer_ip = %peer.ip,
                    peer_port = peer.port,
                    error = %e,
                    "Failed to connect to discovered peer"
                );
            }
        }
    }

    // Return only connections that were established successfully.
    connected
}

/// Send current-swarm and recently dropped endpoints through peer actors.
///
/// The swarm owns connection membership and drop history, so this is shared
/// by both downloading and seeding rather than depending on a piece loop.
pub(crate) async fn send_periodic_pex_to_swarm(
    swarm: &mut crate::engine::bittorrent::peer::message_handler::PeerSwarm,
    last_pex_send: &mut Instant,
    pex_enabled: bool,
) {
    send_periodic_pex_to_swarm_at(swarm, last_pex_send, pex_enabled, Instant::now()).await;
}

async fn send_periodic_pex_to_swarm_at(
    swarm: &mut crate::engine::bittorrent::peer::message_handler::PeerSwarm,
    last_pex_send: &mut Instant,
    pex_enabled: bool,
    now: Instant,
) {
    if now.saturating_duration_since(*last_pex_send) < PEX_SEND_INTERVAL || !pex_enabled {
        return;
    }

    *last_pex_send = now;
    let mut known_endpoints = HashSet::new();
    let added = swarm
        .iter()
        .filter(|actor| {
            !actor.dead
                && !actor.incoming
                && now.saturating_duration_since(actor.first_contact_time) < PEX_SEND_INTERVAL
        })
        .filter_map(|actor| actor.advertised_endpoint)
        .filter(|endpoint| known_endpoints.insert(*endpoint))
        .map(|endpoint| PeerAddr::new(&endpoint.ip().to_string(), endpoint.port()))
        .collect::<Vec<_>>();
    let dropped = swarm
        .recently_dropped_endpoints()
        .filter(|(_, dropped_at)| now.saturating_duration_since(*dropped_at) < PEX_SEND_INTERVAL)
        .map(|(endpoint, _)| endpoint)
        .filter(|endpoint| !known_endpoints.contains(endpoint))
        .map(|endpoint| PeerAddr::new(&endpoint.ip().to_string(), endpoint.port()))
        .collect::<Vec<_>>();
    if added.is_empty() && dropped.is_empty() {
        return;
    }

    let peers = swarm
        .iter()
        .filter(|actor| !actor.dead)
        .filter_map(|actor| {
            Some((
                actor.actor_id,
                actor.endpoint.ip().to_string(),
                actor.ut_pex_id?,
            ))
        })
        .collect::<Vec<_>>();
    let mut sent_count = 0;
    let mut disconnected = Vec::new();
    for (actor_id, recipient_ip, remote_ut_pex_id) in peers {
        let peer_addrs = added
            .iter()
            .filter(|peer| peer.ip != recipient_ip)
            .take(PexHandler::DEFAULT_MAX_PEERS)
            .cloned()
            .collect::<Vec<_>>();
        let dropped_addrs = dropped
            .iter()
            .filter(|peer| peer.ip != recipient_ip)
            .take(PexHandler::DEFAULT_MAX_PEERS)
            .cloned()
            .collect::<Vec<_>>();
        if peer_addrs.is_empty() && dropped_addrs.is_empty() {
            continue;
        }
        let payload = PexHandler::build_pex_message(&peer_addrs, &dropped_addrs).encode();
        let wire_bytes = serialize_extended(remote_ut_pex_id, payload);
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
    use super::{
        BtPeerSource, PEX_SEND_INTERVAL, connect_discovered_peers, send_periodic_pex_to_swarm,
        send_periodic_pex_to_swarm_at,
    };
    use crate::engine::bittorrent::download::command::BtDownloadCommand;
    use crate::engine::bittorrent::download::execute::piece_download::session::peer_dials::PeerDialConfig;
    use crate::engine::bittorrent::peer::interaction::PEER_CONNECT_SETTLE_TIME;
    use crate::engine::bittorrent::peer::message_handler::{PeerEvent, PeerSwarm};
    use crate::engine::bittorrent::peer::upload_session::{
        InMemoryPieceProvider, PieceDataProvider,
    };
    use crate::request::request_group::{DownloadOptions, GroupId};
    use aria2_protocol::bittorrent::extension::pex::{PexHandler, PexMessage};
    use aria2_protocol::bittorrent::message::types::BtMessage;
    use aria2_protocol::bittorrent::peer::connection::PeerAddr;
    use aria2_protocol::bittorrent::peer::connection::PeerConnection;
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    async fn make_incoming_connection(
        listener: tokio::net::TcpListener,
        peer_id: [u8; 20],
        source_ip: Ipv4Addr,
    ) -> (tokio::net::TcpStream, std::net::SocketAddr, PeerConnection) {
        let remote_socket = tokio::net::TcpSocket::new_v4().unwrap();
        remote_socket
            .bind(std::net::SocketAddr::new(source_ip.into(), 0))
            .unwrap();
        let remote = remote_socket
            .connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (local, endpoint) = listener.accept().await.unwrap();
        let connection = PeerConnection::from_stream_with_peer(local, peer_id, false, true);
        (remote, endpoint, connection)
    }

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
                    && let Ok(connection) = incoming.complete([8; 20], None, false).await
                {
                    return connection;
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
                    && let Ok(connection) = incoming.complete([10; 20], None, false).await
                {
                    return connection;
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

    #[tokio::test]
    async fn periodic_pex_sends_removed_swarm_peers_to_actor_connections() {
        let live_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (live_remote_stream, live_endpoint, live_transport) =
            make_incoming_connection(live_listener, [1; 20], Ipv4Addr::new(127, 0, 0, 2)).await;
        let mut live_connection =
            crate::engine::bittorrent::peer::connection::BtPeerConn::from_incoming_tcp(
                live_transport,
                live_endpoint,
            );
        live_connection.incoming = false;
        live_connection.allocate_session_resource(16, 1, 16);
        live_connection.register_peer_extension("ut_pex", 19);
        let live_actor_id = live_connection.actor_id;

        let dropped_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (dropped_remote_stream, dropped_endpoint, dropped_transport) =
            make_incoming_connection(dropped_listener, [2; 20], Ipv4Addr::new(127, 0, 0, 3)).await;
        let mut dropped_connection =
            crate::engine::bittorrent::peer::connection::BtPeerConn::from_incoming_tcp(
                dropped_transport,
                dropped_endpoint,
            );
        dropped_connection.incoming = false;
        let dropped_actor_id = dropped_connection.actor_id;

        let fresh_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (fresh_remote_stream, fresh_endpoint, fresh_transport) =
            make_incoming_connection(fresh_listener, [3; 20], Ipv4Addr::new(127, 0, 0, 4)).await;
        let mut fresh_connection =
            crate::engine::bittorrent::peer::connection::BtPeerConn::from_incoming_tcp(
                fresh_transport,
                fresh_endpoint,
            );
        fresh_connection.incoming = false;
        let fresh_actor_id = fresh_connection.actor_id;

        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut swarm = PeerSwarm::new(8);
        assert!(
            swarm
                .spawn_peer(live_connection, None, Arc::clone(&provider))
                .is_ok()
        );
        assert!(swarm.spawn_peer(dropped_connection, None, provider).is_ok());
        assert!(
            swarm
                .spawn_peer(
                    fresh_connection,
                    None,
                    Arc::new(InMemoryPieceProvider::new(16, 1)),
                )
                .is_ok()
        );

        swarm.apply_event(&PeerEvent::GracefulDisconnected {
            actor_id: dropped_actor_id,
        });
        let removed = swarm.remove_dead().await;
        assert!(
            removed
                .iter()
                .any(|(_, endpoint)| *endpoint == dropped_endpoint)
        );
        assert!(
            swarm
                .recently_dropped_endpoints()
                .any(|(addr, _)| addr == dropped_endpoint)
        );
        assert_eq!(swarm.actor(live_actor_id).unwrap().ut_pex_id, Some(19));

        let mut last_pex_send = Instant::now() - PEX_SEND_INTERVAL;
        send_periodic_pex_to_swarm(&mut swarm, &mut last_pex_send, true).await;
        assert!(last_pex_send.elapsed() < Duration::from_secs(1));

        let mut remote =
            PeerConnection::from_stream_with_peer(live_remote_stream, [10; 20], false, false);
        let message = tokio::time::timeout(Duration::from_secs(1), remote.read_message())
            .await
            .expect("live actor should send the scheduled PEX message")
            .unwrap()
            .unwrap();
        let BtMessage::Extended { ext_id, payload } = message else {
            panic!("expected an extended PEX message, got {message:?}");
        };
        assert_eq!(ext_id, 19);
        let PexMessage::Added { dropped, .. } = PexHandler::parse_pex_data(&payload).unwrap()
        else {
            panic!("expected a BEP 11 added/dropped payload");
        };
        assert!(dropped.iter().any(|peer| {
            peer.ip == dropped_endpoint.ip().to_string() && peer.port == dropped_endpoint.port()
        }));
        assert!(!dropped.iter().any(|peer| peer.ip == "127.0.0.2"));
        assert!(swarm.actor(live_actor_id).is_some());

        let dropped_at = swarm
            .recently_dropped_endpoints()
            .find_map(|(endpoint, dropped_at)| (endpoint == dropped_endpoint).then_some(dropped_at))
            .expect("graceful outgoing peer should have a timestamped PEX drop");
        let expiry = dropped_at + PEX_SEND_INTERVAL + Duration::from_secs(1);
        swarm
            .actor_mut(fresh_actor_id)
            .expect("fresh advertising peer remains in the swarm")
            .first_contact_time = expiry;
        let mut expiry_last_pex_send = dropped_at;
        send_periodic_pex_to_swarm_at(&mut swarm, &mut expiry_last_pex_send, true, expiry).await;
        let PexMessage::Added { peers, dropped, .. } =
            tokio::time::timeout(Duration::from_secs(1), remote_read_pex(&mut remote))
                .await
                .expect("fresh PEX should still be sent after dropped-peer expiry")
        else {
            panic!("expected a BEP 11 added/dropped payload after expiry");
        };
        assert!(peers.iter().any(|peer| {
            peer.addr.ip == fresh_endpoint.ip().to_string()
                && peer.addr.port == fresh_endpoint.port()
        }));
        assert!(!dropped.iter().any(|peer| {
            peer.ip == dropped_endpoint.ip().to_string() && peer.port == dropped_endpoint.port()
        }));

        swarm.shutdown_all().await;
        drop(dropped_remote_stream);
        drop(fresh_remote_stream);
    }

    async fn remote_read_pex(remote: &mut PeerConnection) -> PexMessage {
        loop {
            let message = remote
                .read_message()
                .await
                .expect("PEX actor stream should remain readable")
                .expect("PEX actor should not close the connection");
            if let BtMessage::Extended {
                ext_id: 19,
                payload,
            } = message
            {
                return PexHandler::parse_pex_data(&payload)
                    .expect("actor should emit a valid PEX payload");
            }
        }
    }
}

#[cfg(test)]
#[path = "pex/bep10_port_tests.rs"]
mod bep10_port_tests;
