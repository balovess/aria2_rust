use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_peer_interaction::BtPeerInteraction;
use crate::error::Result;
use crate::util::rwlock_ext::RwLockRecover;

use super::super::super::types::{EndgameState, PeerKey};

pub(super) struct NewPeerConnectionsContext<'a> {
    pub(super) peer_last_data_time: &'a mut HashMap<PeerKey, Instant>,
    pub(super) pex_enabled_peers: &'a mut HashSet<PeerKey>,
    pub(super) allowed_fast_sent_peers: &'a mut HashMap<PeerKey, HashSet<u32>>,
    pub(super) suggest_sent_counts: &'a mut HashMap<PeerKey, usize>,
    pub(super) peer_tracker: &'a mut crate::engine::bt_piece::PeerBitfieldTracker,
    pub(super) choking_algo: &'a mut Option<crate::engine::choking_algorithm::ChokingAlgorithm>,
}

pub(super) enum PeerWaitEvent {
    Incoming(crate::engine::bt_peer_listener::IncomingPeer),
    PeerMessage {
        index: usize,
        result: Result<Option<aria2_protocol::bittorrent::message::types::BtMessage>>,
    },
    Wake,
}

impl BtDownloadCommand {
    pub(super) fn next_peer_event_deadline(
        &self,
        active_connections: &[BtPeerConn],
        stop_timeout_deadline: Option<Instant>,
    ) -> Instant {
        let now = Instant::now();
        let mut deadline = now + Duration::from_secs(24 * 60 * 60);

        if self.dht_engine.is_some()
            && let Some(delay) = self
                .dht_periodic_lookup
                .next_lookup_delay(active_connections.len())
        {
            deadline = deadline.min(now + delay);
        }
        if let Some(delay) = self
            .tracker_announcer
            .as_ref()
            .and_then(|announcer| announcer.next_default_announce_delay())
        {
            deadline = deadline.min(now + delay);
        }
        if let Some(stop_timeout_deadline) = stop_timeout_deadline {
            deadline = deadline.min(stop_timeout_deadline);
        }
        for connection in active_connections {
            deadline = deadline.min(connection.keepalive_deadline());
        }
        deadline
    }

    pub(super) async fn send_due_keepalives(active_connections: &mut [BtPeerConn]) {
        for connection in active_connections {
            if connection.should_send_keepalive()
                && let Err(error) = connection.send_keepalive().await
            {
                tracing::debug!(
                    peer = %format!("{}:{}", connection.ip_addr, connection.port),
                    %error,
                    "Failed to send configured BitTorrent keep-alive"
                );
            }
        }
    }

    /// Wait for a peer/discovery event instead of waking on a fixed short
    /// delay. Network messages are read concurrently from all active peers;
    /// tracker and DHT timers are only used at their protocol deadlines.
    pub(super) async fn wait_for_peer_event(
        &mut self,
        active_connections: &mut [BtPeerConn],
        deadline: Instant,
    ) -> PeerWaitEvent {
        let completion_notify = self.dht_periodic_lookup.completion_notifier();
        let completion_wait = completion_notify.notified();
        let lifecycle_notify = self.group.recover().lifecycle_notifier();
        let lifecycle_wait = lifecycle_notify.notified();
        let mut incoming_receiver = self.incoming_peers.take();
        let deadline_wait = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
        tokio::pin!(deadline_wait);

        let mut peer_reads = active_connections
            .iter_mut()
            .enumerate()
            .map(|(index, connection)| async move { (index, connection.read_message().await) })
            .collect::<futures::stream::FuturesUnordered<_>>();

        let event = tokio::select! {
            incoming = async {
                match incoming_receiver.as_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending::<Option<crate::engine::bt_peer_listener::IncomingPeer>>().await,
                }
            } => match incoming {
                Some(incoming) => PeerWaitEvent::Incoming(incoming),
                None => {
                    incoming_receiver = None;
                    PeerWaitEvent::Wake
                }
            },
            peer = peer_reads.next(), if !peer_reads.is_empty() => {
                peer.map_or(PeerWaitEvent::Wake, |(index, result)| {
                    PeerWaitEvent::PeerMessage { index, result }
                })
            },
            _ = completion_wait => PeerWaitEvent::Wake,
            _ = lifecycle_wait => PeerWaitEvent::Wake,
            _ = &mut deadline_wait => PeerWaitEvent::Wake,
        };

        drop(peer_reads);
        self.incoming_peers = incoming_receiver;
        event
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn apply_peer_wait_event(
        event: PeerWaitEvent,
        active_connections: &mut Vec<BtPeerConn>,
        peer_tracker: &mut crate::engine::bt_piece::PeerBitfieldTracker,
        pex_enabled_peers: &mut HashSet<PeerKey>,
        peer_last_data_time: &mut HashMap<PeerKey, Instant>,
        allowed_fast_sent_peers: &mut HashMap<PeerKey, HashSet<u32>>,
        suggest_sent_counts: &mut HashMap<PeerKey, usize>,
        endgame_state: &mut EndgameState,
        choking_algo: Option<&mut crate::engine::choking_algorithm::ChokingAlgorithm>,
        peer_storage: &std::sync::Arc<
            std::sync::Mutex<crate::engine::bt_peer_storage::DefaultPeerStorage>,
        >,
    ) -> Option<crate::engine::bt_peer_listener::IncomingPeer> {
        match event {
            PeerWaitEvent::Incoming(incoming) => return Some(incoming),
            PeerWaitEvent::PeerMessage { index, result } => {
                let failed_address = {
                    let connection = active_connections.get_mut(index)?;
                    let peer_key = PeerKey::from_peer(&connection.ip_addr, connection.port);
                    match result {
                        Ok(Some(message)) => {
                            let before = connection
                                .session_resource
                                .as_ref()
                                .map(|resource| resource.bitfield().to_vec());
                            match message {
                                aria2_protocol::bittorrent::message::types::BtMessage::Have {
                                    piece_index,
                                } => connection.update_peer_bitfield(piece_index as usize, 1),
                                aria2_protocol::bittorrent::message::types::BtMessage::Bitfield {
                                    data,
                                } => connection.set_peer_bitfield(&data),
                                aria2_protocol::bittorrent::message::types::BtMessage::HaveAll => {
                                    connection.mark_seeder()
                                }
                                aria2_protocol::bittorrent::message::types::BtMessage::HaveNone => {
                                    connection.set_peer_bitfield(&[])
                                }
                                aria2_protocol::bittorrent::message::types::BtMessage::Choke => {
                                    connection.stats.peer_choking = true;
                                }
                                aria2_protocol::bittorrent::message::types::BtMessage::Unchoke => {
                                    connection.stats.peer_choking = false;
                                }
                                _ => {}
                            }
                            let after = connection
                                .session_resource
                                .as_ref()
                                .map(|resource| resource.bitfield().to_vec());
                            if before != after
                                && let (Some(peer_key), Some(bitfield)) =
                                    (peer_key, after.as_deref())
                            {
                                peer_tracker.update_peer_bitfield(
                                    &BtPeerInteraction::peer_tracker_key(connection),
                                    bitfield,
                                );
                                peer_last_data_time.insert(peer_key, Instant::now());
                            }
                            None
                        }
                        Ok(None) | Err(_) => {
                            connection.disconnected_gracefully = true;
                            connection.remote_endpoint()
                        }
                    }
                };
                if let Some(address) = failed_address {
                    Self::remove_failed_peers(
                        active_connections,
                        &[address],
                        choking_algo,
                        pex_enabled_peers,
                        peer_last_data_time,
                        allowed_fast_sent_peers,
                        suggest_sent_counts,
                        endgame_state,
                        peer_tracker,
                        peer_storage,
                    );
                }
            }
            PeerWaitEvent::Wake => {}
        }
        None
    }

    // Parameters are individually meaningful; grouping into a struct would
    // reduce clarity for this inner download loop.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn remove_failed_peers(
        active_connections: &mut Vec<BtPeerConn>,
        failed_peers: &[std::net::SocketAddr],
        choking_algo: Option<&mut crate::engine::choking_algorithm::ChokingAlgorithm>,
        pex_enabled_peers: &mut std::collections::HashSet<PeerKey>,
        peer_last_data_time: &mut HashMap<PeerKey, Instant>,
        allowed_fast_sent_peers: &mut HashMap<PeerKey, std::collections::HashSet<u32>>,
        suggest_sent_counts: &mut HashMap<PeerKey, usize>,
        endgame_state: &mut EndgameState,
        peer_tracker: &mut crate::engine::bt_piece::PeerBitfieldTracker,
        peer_storage: &std::sync::Arc<
            std::sync::Mutex<crate::engine::bt_peer_storage::DefaultPeerStorage>,
        >,
    ) {
        if failed_peers.is_empty() {
            return;
        }
        let failed: HashSet<_> = failed_peers.iter().copied().collect();
        let removed_indices: Vec<_> = active_connections
            .iter()
            .enumerate()
            .filter_map(|(index, conn)| {
                let address = format!("{}:{}", conn.ip_addr, conn.port).parse().ok()?;
                failed.contains(&address).then_some(index)
            })
            .collect();
        if removed_indices.is_empty() {
            return;
        }
        for &index in removed_indices.iter().rev() {
            if let Some(conn) = active_connections.get(index) {
                peer_tracker.remove_peer(&BtPeerInteraction::peer_tracker_key(conn));
            }
        }
        for &index in removed_indices.iter().rev() {
            active_connections[index].release_session_resource();
        }
        if let Some(algo) = choking_algo {
            algo.remove_peers(removed_indices.as_slice());
        }
        let removed_keys: Vec<_> = removed_indices
            .iter()
            .filter_map(|&index| active_connections.get(index))
            .filter_map(|conn| PeerKey::from_peer(&conn.ip_addr, conn.port))
            .collect();
        endgame_state.remove_peers(&removed_keys);
        let mut removed = Vec::new();
        active_connections.retain(|conn| {
            let address =
                match format!("{}:{}", conn.ip_addr, conn.port).parse::<std::net::SocketAddr>() {
                    Ok(address) => address,
                    Err(_) => return true,
                };
            if failed.contains(&address) {
                removed.push(address);
                false
            } else {
                true
            }
        });
        if removed.is_empty() {
            return;
        }
        {
            let mut storage = peer_storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for address in &removed {
                storage.return_peer_by_endpoint(&address.ip().to_string(), address.port());
            }
        }
        for peer_key in &removed_keys {
            pex_enabled_peers.remove(peer_key);
            peer_last_data_time.remove(peer_key);
            allowed_fast_sent_peers.remove(peer_key);
            suggest_sent_counts.remove(peer_key);
        }
    }

    pub(super) fn append_new_connections(
        active_connections: &mut Vec<BtPeerConn>,
        mut new_connections: Vec<BtPeerConn>,
        max_peers: usize,
        is_private: bool,
        context: &mut NewPeerConnectionsContext<'_>,
        peer_storage: &std::sync::Arc<
            std::sync::Mutex<crate::engine::bt_peer_storage::DefaultPeerStorage>,
        >,
        caretaker_id: u64,
    ) -> usize {
        new_connections.retain(|conn| {
            let Some(endpoint) = conn.remote_endpoint() else {
                tracing::debug!("[BT] Dropping new peer without a remote endpoint");
                return false;
            };
            if endpoint.ip().is_unspecified() || endpoint.port() == 0 {
                tracing::debug!(peer = %endpoint, "[BT] Dropping new peer with invalid endpoint");
                return false;
            }
            true
        });
        let checkout_limit = if max_peers == 0 {
            usize::MAX
        } else {
            max_peers.saturating_sub(active_connections.len())
        };
        let mut seen_endpoints = HashSet::with_capacity(new_connections.len());
        new_connections.retain(|conn| {
            let Some(endpoint) = conn.remote_endpoint() else {
                return false;
            };
            seen_endpoints.insert((endpoint.ip(), endpoint.port()))
        });

        let mut checked_out_endpoints = Vec::with_capacity(new_connections.len());
        {
            let mut storage = peer_storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            new_connections.retain(|conn| {
                let Some(endpoint) = conn.remote_endpoint() else {
                    return false;
                };
                let entry = crate::engine::bt_peer_storage::PeerEntry::new(
                    endpoint.ip().to_string(),
                    endpoint.port(),
                );
                if checked_out_endpoints.len() >= checkout_limit
                    || storage.add_and_checkout_peer(entry, caretaker_id).is_none()
                {
                    return false;
                }
                checked_out_endpoints.push((endpoint.ip().to_string(), endpoint.port()));
                true
            });
        }

        let previous_len = active_connections.len();
        active_connections.extend(new_connections);
        let connected = active_connections.len() - previous_len;
        {
            let mut storage = peer_storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (ip, port) in &checked_out_endpoints {
                storage.set_peer_active(ip, *port, true);
            }
        }
        if connected == 0 {
            return 0;
        }

        tracing::debug!(connected, "[BT] Added new peer connections");

        for conn in &active_connections[previous_len..] {
            let Some(peer_key) = PeerKey::from_peer(&conn.ip_addr, conn.port) else {
                continue;
            };
            context.peer_last_data_time.insert(peer_key, Instant::now());
            context.allowed_fast_sent_peers.entry(peer_key).or_default();
            context.suggest_sent_counts.entry(peer_key).or_insert(0);
            if !is_private {
                context.pex_enabled_peers.insert(peer_key);
            }
            let bitfield = conn
                .session_resource
                .as_ref()
                .map_or(&[][..], |resource| resource.bitfield());
            context
                .peer_tracker
                .update_peer_bitfield(&BtPeerInteraction::peer_tracker_key(conn), bitfield);
        }

        if let Some(algo) = context.choking_algo.as_mut() {
            for conn in &active_connections[previous_len..] {
                algo.add_peer(conn.stats.clone());
            }
        }
        connected
    }
}
