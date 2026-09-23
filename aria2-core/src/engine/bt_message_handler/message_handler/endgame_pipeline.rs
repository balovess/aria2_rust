//! Event-driven endgame block scheduling over the shared peer workers.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::warn;

use crate::constants;
use crate::engine::bt_download_execute::{EndgameState, types::PeerKey};
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::choking_algorithm::{ChokingAlgorithm, PeerIdentity};
use crate::error::{Aria2Error, FatalError, Result};
use crate::request::request_group::AtomicProgress;

use super::super::types::{BLOCK_SIZE, PeerDownloadBytes, PieceDownloadResult};
use super::BtMessageHandler;
use super::normal_pipeline::piece_attempt_budget_exhausted;
use super::peer_worker::{PeerCommand, PeerEvent, PeerWorkers, apply_interest_change};
use super::pipelined::BlockRequest;

impl BtMessageHandler {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn download_piece_blocks_endgame_pipelined_with_sources_and_activity(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
        endgame_state: &mut EndgameState,
        dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        upload_provider: Option<Arc<dyn crate::engine::bt_upload_session::PieceDataProvider>>,
        network_activity: Option<&AtomicProgress>,
        request_timeout: Duration,
        max_attempts: u32,
        mut choking_algo: Option<&mut ChokingAlgorithm>,
    ) -> Result<PieceDownloadResult> {
        let block_count = piece_length.div_ceil(BLOCK_SIZE);
        if num_blocks != block_count {
            warn!(
                piece_index,
                requested_blocks = num_blocks,
                actual_blocks = block_count,
                "BT endgame block count disagrees with piece length; using the actual layout"
            );
        }

        let mut attempts = 0u32;
        loop {
            attempts = attempts.saturating_add(1);
            let peer_addresses = connections
                .iter()
                .map(|connection| {
                    connection.remote_endpoint().or_else(|| {
                        let ip = connection.ip_addr.parse().ok()?;
                        Some(SocketAddr::new(ip, connection.port))
                    })
                })
                .collect::<Vec<_>>();
            let peer_indices = peer_addresses
                .iter()
                .enumerate()
                .filter_map(|(index, address)| {
                    address.map(|address| (PeerKey::new(address), index))
                })
                .collect::<HashMap<_, _>>();
            let peer_identity_indices = connections
                .iter()
                .enumerate()
                .map(|(index, connection)| (PeerIdentity::from(&connection.stats), index))
                .collect::<HashMap<_, _>>();
            let event_capacity = connections.len().saturating_mul(8).max(64);
            let (event_tx, mut event_rx) = mpsc::channel(event_capacity);
            let mut workers = PeerWorkers::new(
                connections,
                event_tx,
                dht_engine.clone(),
                upload_provider.clone(),
            );
            let mut live = vec![true; peer_addresses.len()];
            let mut piece_data = vec![0u8; piece_length as usize];
            let mut peer_bytes = Vec::<PeerDownloadBytes>::new();
            let mut failed_peers = Vec::new();
            let mut complete = true;

            for block_index in 0..block_count {
                let offset = block_index * BLOCK_SIZE;
                let request = BlockRequest {
                    block_index,
                    offset,
                    length: (piece_length - offset).min(BLOCK_SIZE),
                };
                let mut requested = vec![false; peer_addresses.len()];
                let mut rejected = vec![false; peer_addresses.len()];

                for peer_index in 0..peer_addresses.len() {
                    if !live[peer_index] {
                        continue;
                    }
                    let sent = workers.senders[peer_index].as_ref().is_some_and(|sender| {
                        sender
                            .try_send(PeerCommand::Request {
                                piece_index,
                                request,
                            })
                            .is_ok()
                    });
                    if sent {
                        requested[peer_index] = true;
                        if let Some(address) = peer_addresses[peer_index] {
                            endgame_state.track_request(
                                piece_index,
                                request.offset,
                                request.length,
                                PeerKey::new(address),
                            );
                        }
                    } else {
                        live[peer_index] = false;
                        if let Some(address) = peer_addresses[peer_index]
                            && !failed_peers.contains(&address)
                        {
                            failed_peers.push(address);
                        }
                    }
                }

                let mut winner = None;
                let deadline = Instant::now() + request_timeout;
                while winner.is_none() {
                    if !(0..live.len())
                        .any(|index| live[index] && requested[index] && !rejected[index])
                    {
                        break;
                    }
                    let wait = deadline.saturating_duration_since(Instant::now());
                    if wait.is_zero() {
                        break;
                    }
                    let workers_active = !workers.workers.is_empty();
                    tokio::select! {
                        event = event_rx.recv() => {
                            let Some(event) = event else { break };
                            match event {
                                PeerEvent::UploadBytes { actor_id, .. } => {
                                    if workers.peer_index(actor_id).is_none() {
                                        continue;
                                    }
                                }
                                PeerEvent::InterestChanged { actor_id, snapshot } => {
                                    if workers.peer_index(actor_id).is_none() {
                                        continue;
                                    }
                                    apply_interest_change(
                                        &mut workers,
                                        &peer_identity_indices,
                                        choking_algo.as_deref_mut(),
                                        *snapshot,
                                    ).await;
                                }
                                PeerEvent::Message { actor_id, message } => {
                                    let Some(peer_index) = workers.peer_index(actor_id) else {
                                        continue;
                                    };
                                    tracing::trace!(actor_id = actor_id.0, peer_index, "Received BT peer event");
                                    use aria2_protocol::bittorrent::message::types::BtMessage;
                                    match message {
                                        BtMessage::Piece { index, begin, data }
                                            if index == piece_index
                                                && begin == request.offset
                                                && requested.get(peer_index).copied().unwrap_or(false) =>
                                        {
                                            if data.len() == request.length as usize {
                                                winner = Some((peer_index, data));
                                            } else {
                                                rejected[peer_index] = true;
                                                warn!(piece_index, offset = begin, expected = request.length,
                                                    actual = data.len(), "Discarding malformed BT endgame block");
                                            }
                                        }
                                        BtMessage::Reject { index, offset, .. }
                                            if index == piece_index && offset == request.offset => {
                                            rejected[peer_index] = true;
                                        }
                                        _ => {}
                                    }
                                }
                                PeerEvent::RequestFailed { actor_id, .. }
                                | PeerEvent::Disconnected { actor_id } => {
                                    let Some(peer_index) = workers.peer_index(actor_id) else {
                                        continue;
                                    };
                                    live[peer_index] = false;
                                    if let Some(address) = peer_addresses[peer_index]
                                        && !failed_peers.contains(&address)
                                    {
                                        failed_peers.push(address);
                                    }
                                }
                            }
                        }
                        worker = workers.workers.next(), if workers_active => {
                            if let Some(actor_id) = worker
                                && let Some(peer_index) = workers.peer_index(actor_id)
                            {
                                live[peer_index] = false;
                                if let Some(address) = peer_addresses[peer_index]
                                    && !failed_peers.contains(&address)
                                {
                                    failed_peers.push(address);
                                }
                            }
                        }
                        _ = tokio::time::sleep(wait) => break,
                    }
                }

                if let Some((winner_index, data)) = winner {
                    if !data.is_empty()
                        && let Some(progress) = network_activity
                    {
                        progress.record_network_activity();
                    }
                    let start = request.offset as usize;
                    piece_data[start..start + data.len()].copy_from_slice(&data);
                    if let Some(address) = peer_addresses[winner_index] {
                        let bytes = data.len() as u64;
                        if let Some(entry) = peer_bytes
                            .iter_mut()
                            .find(|entry| entry.peer_index == winner_index)
                        {
                            entry.bytes += bytes;
                        } else {
                            peer_bytes.push(PeerDownloadBytes {
                                peer_index: winner_index,
                                peer: address,
                                bytes,
                            });
                        }
                    }
                    let winner_key = peer_addresses[winner_index].map(PeerKey::new);
                    let cancel_targets = winner_key
                        .map(|key| {
                            endgame_state.take_cancel_targets(
                                piece_index,
                                request.offset,
                                request.length,
                                key,
                            )
                        })
                        .unwrap_or_default();
                    for key in cancel_targets {
                        if let Some(&peer_index) = peer_indices.get(&key)
                            && peer_index != winner_index
                            && let Some(sender) = workers.senders[peer_index].as_ref()
                        {
                            let _ = sender
                                .send(PeerCommand::Cancel {
                                    piece_index,
                                    request,
                                })
                                .await;
                        }
                    }
                } else {
                    let targets = endgame_state.get_cancel_targets(
                        piece_index,
                        request.offset,
                        request.length,
                    );
                    endgame_state.remove_request(piece_index, request.offset, request.length);
                    for key in targets {
                        if let Some(&peer_index) = peer_indices.get(&key)
                            && let Some(sender) = workers.senders[peer_index].as_ref()
                        {
                            let _ = sender
                                .send(PeerCommand::Cancel {
                                    piece_index,
                                    request,
                                })
                                .await;
                        }
                    }
                    complete = false;
                    break;
                }
            }

            workers.shutdown(&mut event_rx).await;
            drop(workers);

            if complete {
                return Ok(PieceDownloadResult {
                    data: piece_data,
                    peer_bytes,
                    failed_peers,
                });
            }
            if piece_attempt_budget_exhausted(attempts, max_attempts) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(constants::BT_RETRY_DELAY_MS)).await;
        }

        Err(Aria2Error::Fatal(FatalError::Config(format!(
            "Failed to download piece {} after {} endgame attempts",
            piece_index,
            if max_attempts == 0 {
                attempts
            } else {
                max_attempts
            }
        ))))
    }
}
