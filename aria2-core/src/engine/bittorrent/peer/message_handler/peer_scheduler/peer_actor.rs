//! Long-lived peer I/O actors and piece-scoped actor command sets.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::trace;

use crate::engine::bittorrent::peer::choking_algorithm::{
    ChokingAlgorithm, IdentityChokeAction, PeerIdentity,
};
use crate::engine::bittorrent::peer::connection::{BtPeerConn, PeerActorId};
use crate::error::Result;

use super::normal::process_pex_during_read;
use super::peer_registry::{PeerSwarm, PeerSwarmEventLease};
use super::peer_request::PeerRequestLedger;
pub(super) use super::peer_request::RequestGeneration;
use super::peer_snapshot::PeerSchedulingSnapshot;
use super::pipelined::BlockRequest;

const METADATA_PIECE_SIZE: usize = 16 * 1024;
const PEER_ACTOR_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
#[path = "peer_actor/control.rs"]
mod control;
pub(crate) use control::{
    PeerActorCommandReceiver, PeerActorControl, PeerActorPayloadConfig, PeerActorTask, PeerCommand,
    PeerEvent,
};
pub(super) use control::{local_metadata_response, notify_queue_capacity};

#[path = "peer_actor/generation.rs"]
mod generation;
pub(super) use generation::{PeerGeneration, TryRequestError};

#[path = "peer_actor/incoming.rs"]
mod incoming;
use incoming::{IncomingMessageAction, IncomingMessageContext};

#[path = "peer_actor/startup.rs"]
mod startup;
use startup::initialize_peer_actor;

pub(crate) async fn run_peer_actor(
    actor_id: PeerActorId,
    connection: &mut BtPeerConn,
    command_rx: PeerActorCommandReceiver,
    event_tx: mpsc::Sender<PeerEvent>,
    dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    mut upload_provider: Option<
        Arc<dyn crate::engine::bittorrent::peer::upload_session::PieceDataProvider>,
    >,
    pending_download_requests: Arc<AtomicUsize>,
) -> PeerActorId {
    let PeerActorCommandReceiver {
        mut commands,
        mut generation_updates,
        mut desired_state_updates,
        capacity_updates,
    } = command_rx;
    desired_state_updates.mark_changed();
    let (extension_handshake_info, mut availability_sent) =
        match initialize_peer_actor(connection, dht_engine.as_ref(), upload_provider.as_deref())
            .await
        {
            Ok(state) => state,
            Err(error) => {
                tracing::debug!(actor_id = actor_id.0, %error, "BT peer actor startup failed");
                let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                return actor_id;
            }
        };
    let mut wanted_pieces: Arc<[u8]> = Arc::from([]);
    let mut requests = PeerRequestLedger::default();
    let mut active_generations = HashMap::<u32, RequestGeneration>::new();
    let mut upload_flush_deadline: Option<tokio::time::Instant> = None;
    let mut upload_rate_changes = connection.upload_rate_change_receivers();
    let mut shutdown_requested = false;
    let mut local_seeder = false;
    let local_ut_metadata_id =
        aria2_protocol::bittorrent::message::extension::ExtensionHandshake::new()
            .ut_metadata_id()
            .unwrap_or(1);
    loop {
        pending_download_requests.store(requests.len(), Ordering::Relaxed);
        if shutdown_requested && !connection.has_pending_upload_messages() {
            break;
        }
        if connection.has_pending_upload_messages() {
            let delay = connection
                .pending_upload_flush_delay()
                .unwrap_or(Duration::ZERO);
            let delay = if delay.is_zero() {
                Duration::from_millis(2)
            } else {
                delay
            };
            upload_flush_deadline.get_or_insert_with(|| tokio::time::Instant::now() + delay);
        } else {
            upload_flush_deadline = None;
        }
        let keepalive_deadline = tokio::time::Instant::from_std(connection.keepalive_deadline());
        let peer_timeout_deadline =
            tokio::time::Instant::from_std(connection.peer_timeout_deadline());
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(peer_timeout_deadline) => {
                tracing::debug!(actor_id = actor_id.0, "BT peer inactivity timeout elapsed");
                let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                break;
            }
            _ = tokio::time::sleep_until(keepalive_deadline) => {
                if let Err(error) = connection.send_keepalive().await {
                    tracing::debug!(actor_id = actor_id.0, %error, "Failed to send BT peer keep-alive");
                    let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                    break;
                }
            }
            generation_update = generation_updates.changed() => {
                if generation_update.is_err() {
                    break;
                }
                let previous_request_count = requests.len();
                let latest_generations = {
                    let active = generation_updates.borrow_and_update();
                    active.clone()
                };
                let ended_generations = active_generations
                    .iter()
                    .filter_map(|(&piece_index, &generation)| {
                        (!latest_generations.contains_key(&piece_index))
                            .then_some((piece_index, generation))
                    })
                    .collect::<Vec<_>>();
                for (piece_index, generation) in ended_generations {
                    for (request_piece, request) in requests.drain_piece_generation(piece_index, generation) {
                        if connection
                            .send_cancel(&request.message(request_piece))
                            .await
                            .is_ok()
                        {
                            connection.record_outbound_activity();
                        }
                    }
                    active_generations.remove(&piece_index);
                }
                for (&piece_index, &generation) in &latest_generations {
                    if active_generations
                        .get(&piece_index)
                        .is_some_and(|active| active.is_at_least(generation))
                    {
                        continue;
                    }
                    for (request_piece, request) in requests.drain_piece_before_generation(piece_index, generation) {
                        if connection
                            .send_cancel(&request.message(request_piece))
                            .await
                            .is_ok()
                        {
                            connection.record_outbound_activity();
                        }
                    }
                    active_generations.insert(piece_index, generation);
                }
                pending_download_requests.store(requests.len(), Ordering::Relaxed);
                if requests.len() != previous_request_count
                    && event_tx
                        .send(PeerEvent::OutstandingDownloadRequests {
                            actor_id,
                            count: requests.len(),
                        })
                        .await
                        .is_err()
                {
                    break;
                }
            }
            desired_state_update = desired_state_updates.changed() => {
                if desired_state_update.is_err() {
                    break;
                }
                let desired_state = {
                    let latest_desired_state = desired_state_updates.borrow_and_update();
                    latest_desired_state.clone()
                };
                wanted_pieces = Arc::clone(&desired_state.wanted_pieces);
                local_seeder = desired_state.local_seeder;
                match reconcile_peer_interest(
                    actor_id,
                    connection,
                    &wanted_pieces,
                    &event_tx,
                ).await {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(error) => {
                        tracing::debug!(actor_id = actor_id.0, %error, "Failed to update BT peer interest");
                        let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                        break;
                    }
                }
                match reconcile_peer_choke_state(
                    actor_id,
                    connection,
                    desired_state.choke_upload,
                    &event_tx,
                ).await {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(error) => {
                        tracing::debug!(actor_id = actor_id.0, %error, "Failed to update BT peer choke state");
                        let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                        break;
                    }
                }
                if local_seeder && connection.is_seeder() {
                    tracing::debug!(actor_id = actor_id.0, "Closing BT connection between seeders");
                    let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                    break;
                }
            }
            command = commands.recv(), if !shutdown_requested => {
                if command.is_some() {
                    notify_queue_capacity(&capacity_updates);
                }
                match command {
                    Some(PeerCommand::RequestMetadata { piece }) => {
                        if !connection.is_metadata_pending() {
                            continue;
                        }
                        let Some(ext_id) = connection.peer_extension_id("ut_metadata") else {
                            continue;
                        };
                        let message = aria2_protocol::bittorrent::message::types::BtMessage::Extended {
                            ext_id,
                            payload: aria2_protocol::bittorrent::message::extension::UtMetadataMessage::Request { piece }
                                .to_payload(),
                        };
                        if let Err(error) = connection.send_bt_message(&message).await {
                            tracing::debug!(actor_id = actor_id.0, %error, piece, "Failed to request BEP 9 metadata piece");
                            let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                            break;
                        }
                        connection.record_outbound_activity();
                    }
                    Some(PeerCommand::ActivatePayload(config)) => {
                        if !connection.activate_payload_session(
                            config.piece_length,
                            config.num_pieces,
                            config.total_length,
                        ) {
                            tracing::debug!(actor_id = actor_id.0, "Dropping metadata peer after availability state exceeded its bounded buffer");
                            let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                            break;
                        }
                        connection.local_metadata = Some(Arc::clone(&config.local_metadata));
                        if connection.remote_supports_extended_messaging()
                            && let Some((peer_agent, listen_port)) =
                                extension_handshake_info.as_ref()
                        {
                            let metadata_size = u32::try_from(config.local_metadata.len())
                                .ok()
                                .filter(|size| *size > 0);
                            if let Err(error) = connection
                                .send_extension_handshake_with_metadata(
                                    peer_agent,
                                    *listen_port,
                                    metadata_size,
                                )
                                .await
                            {
                                tracing::debug!(actor_id = actor_id.0, %error, "Failed to advertise local BEP 9 metadata after magnet activation");
                                let _ = event_tx
                                    .send(PeerEvent::Disconnected { actor_id })
                                    .await;
                                break;
                            }
                            connection.record_outbound_activity();
                        }
                        let allowed_fast = connection
                            .peer_allowed_fast_set()
                            .iter()
                            .copied()
                            .collect::<Vec<_>>();
                        let mut event_stream_closed = false;
                        for piece_index in allowed_fast {
                            if event_tx
                                .send(PeerEvent::AllowedFast {
                                    actor_id,
                                    piece_index,
                                })
                                .await
                                .is_err()
                            {
                                event_stream_closed = true;
                                break;
                            }
                        }
                        if event_stream_closed {
                            break;
                        }
                        connection.configure_upload_with_auto_unchoke(
                            &config.upload_config,
                            config.upload_limiter.clone(),
                            config.num_pieces,
                            config.piece_length,
                            config.auto_unchoke,
                        );
                        upload_rate_changes = connection.upload_rate_change_receivers();
                        connection.set_upload_counter(Arc::clone(&config.upload_counter));
                        connection.set_upload_progress(Arc::clone(&config.upload_progress));
                        let allowed_fast = if connection.remote_supports_fast_extension() {
                            aria2_protocol::bittorrent::fast_set::compute_fast_set(
                                connection.remote_ip(),
                                config.num_pieces,
                                &config.network_info_hash,
                                10,
                            )
                        } else {
                            Vec::new()
                        };
                        let mut activation_failed = false;
                        for piece_index in allowed_fast {
                            if let Err(error) = connection
                                .send_bt_message(
                                    &aria2_protocol::bittorrent::message::types::BtMessage::AllowedFast {
                                        index: piece_index,
                                    },
                                )
                                .await
                            {
                                tracing::debug!(actor_id = actor_id.0, %error, "Failed to send post-metadata AllowedFast");
                                let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                                activation_failed = true;
                                break;
                            }
                            connection.add_am_allowed_fast(piece_index);
                        }
                        if activation_failed {
                            break;
                        }
                        upload_provider = Some(Arc::clone(&config.provider));
                        if let Err(error) = connection
                            .announce_upload_availability(config.provider.as_ref())
                            .await
                        {
                            tracing::debug!(actor_id = actor_id.0, %error, "Failed to announce availability after metadata resolution");
                            let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                            break;
                        }
                        availability_sent = true;
                        if let Some(resource) = connection.session_resource.as_ref()
                            && event_tx
                                .send(PeerEvent::PeerAvailabilitySnapshot {
                                    actor_id,
                                    bitfield: resource.bitfield().to_vec(),
                                    seeder: connection.seeder,
                                })
                                .await
                                .is_err()
                        {
                            break;
                        }
                        connection.record_outbound_activity();
                    }
                    Some(PeerCommand::Request { generation, piece_index, request }) => {
                        if active_generations.get(&piece_index) != Some(&generation)
                            || requests.contains(piece_index, request)
                        {
                            let _ = event_tx.send(PeerEvent::RequestFailed {
                                actor_id,
                                generation,
                                piece_index,
                                request,
                            }).await;
                            continue;
                        }
                        match connection.send_request(request.message(piece_index)).await {
                            Ok(()) => {
                                requests.record(generation, piece_index, request);
                                pending_download_requests.store(requests.len(), Ordering::Relaxed);
                                if event_tx
                                    .send(PeerEvent::OutstandingDownloadRequests {
                                        actor_id,
                                        count: requests.len(),
                                    })
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                                connection.record_outbound_activity();
                            }
                            Err(error) => {
                                requests.cancel(generation, piece_index, request);
                                tracing::debug!(actor_id = actor_id.0, %error, "Failed to send BT block request");
                                let _ = event_tx
                                    .send(PeerEvent::Disconnected { actor_id })
                                    .await;
                                break;
                            }
                        }
                    }
                    Some(PeerCommand::Cancel { generation, piece_index, request }) => {
                        if active_generations.get(&piece_index) == Some(&generation)
                            && requests.cancel(generation, piece_index, request)
                        {
                            pending_download_requests.store(requests.len(), Ordering::Relaxed);
                            if event_tx
                                .send(PeerEvent::OutstandingDownloadRequests {
                                    actor_id,
                                    count: requests.len(),
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                            if connection.send_cancel(&request.message(piece_index)).await.is_ok() {
                                connection.record_outbound_activity();
                            }
                        }
                    }
                    Some(PeerCommand::HavePiece { piece_index }) => {
                        if let Err(error) = connection.send_have(piece_index).await {
                            tracing::debug!(actor_id = actor_id.0, %error, "Failed to announce validated BT piece");
                            let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                            break;
                        }
                        connection.record_outbound_activity();
                    }
                    Some(PeerCommand::SendPex(wire_bytes)) => {
                        connection.queue_message(wire_bytes);
                        if let Err(error) = connection.flush_send_buffer().await {
                            tracing::debug!(actor_id = actor_id.0, %error, "Failed to send BT PEX message");
                            let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                            break;
                        }
                        connection.record_outbound_activity();
                    }
                    Some(PeerCommand::AnnounceAvailability) => {
                        if availability_sent {
                            continue;
                        }
                        let Some(provider) = upload_provider.as_deref() else {
                            continue;
                        };
                        if let Err(error) = connection.announce_upload_availability(provider).await {
                            tracing::debug!(actor_id = actor_id.0, %error, "Failed to announce BT peer availability");
                            let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                            break;
                        }
                        availability_sent = true;
                    }
                    Some(PeerCommand::Shutdown) | None => {
                        shutdown_requested = true;
                        for (piece_index, request) in requests.drain_all() {
                            if connection
                                .send_cancel(&request.message(piece_index))
                                .await
                                .is_ok()
                            {
                                connection.record_outbound_activity();
                            }
                        }
                        connection.discard_pending_upload_messages();
                        if !connection.has_pending_upload_messages() {
                            break;
                        }
                    }
                }
            }
            message = connection.read_message() => {
                let action = IncomingMessageContext {
                    actor_id,
                    connection: &mut *connection,
                    event_tx: &event_tx,
                    dht_engine: dht_engine.as_ref(),
                    upload_provider: upload_provider.as_ref(),
                    requests: &mut requests,
                    local_seeder,
                    local_ut_metadata_id,
                    wanted_pieces: &wanted_pieces,
                    shutdown_requested,
                    upload_flush_deadline: &mut upload_flush_deadline,
                }
                .handle(message)
                .await;
                if action == IncomingMessageAction::Exit {
                    break;
                }
            }
            _ = async {
                if let Some(deadline) = upload_flush_deadline {
                    tokio::time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if upload_flush_deadline.is_some() => {
                let Some(provider) = upload_provider.as_deref() else {
                    upload_flush_deadline = None;
                    if shutdown_requested {
                        break;
                    }
                    continue;
                };
                let outstanding_upload_count = connection.stats.outstanding_upload_count;
                match connection.flush_upload_messages(provider).await {
                    Ok(bytes) if bytes > 0 => {
                        connection.record_outbound_activity();
                        let Ok(permit) = event_tx.reserve().await else {
                            break;
                        };
                        let recorded_at = Instant::now();
                        permit.send(PeerEvent::UploadBytes {
                            actor_id,
                            bytes,
                            recorded_at,
                            snapshot: Box::new(connection.stats.clone()),
                        });
                    }
                    Ok(_) => {
                        if connection.stats.outstanding_upload_count != outstanding_upload_count
                            && event_tx.send(PeerEvent::UploadQueueChanged {
                                actor_id,
                                snapshot: Box::new(connection.stats.clone()),
                            }).await.is_err()
                        {
                            tracing::debug!(actor_id = actor_id.0, "Peer swarm event receiver closed while publishing flushed upload state");
                            break;
                        }
                    }
                    Err(error) => {
                        tracing::debug!(actor_id = actor_id.0, %error, "Failed to flush queued BT upload messages");
                        let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                        break;
                    }
                }
                let retry_delay = connection
                    .pending_upload_flush_delay()
                    .unwrap_or(Duration::ZERO);
                upload_flush_deadline = connection
                    .has_pending_upload_messages()
                    .then(|| tokio::time::Instant::now() + retry_delay);
                if shutdown_requested {
                    break;
                }
            }
            _ = wait_for_upload_rate_change(&mut upload_rate_changes),
                if connection.has_pending_upload_messages() && upload_rate_changes.is_some() =>
            {
                let retry_delay = connection
                    .pending_upload_flush_delay()
                    .unwrap_or(Duration::ZERO);
                upload_flush_deadline = Some(tokio::time::Instant::now() + retry_delay);
            }
        }
    }

    actor_id
}

#[path = "peer_actor/runtime_support.rs"]
mod runtime_support;
pub(crate) use runtime_support::process_peer_message;
pub(super) use runtime_support::{
    apply_choke_round, apply_interest_change, rebalance_upload_slots_after_peer_disconnect,
};
use runtime_support::{
    reconcile_peer_choke_state, reconcile_peer_interest, wait_for_upload_rate_change,
};

#[cfg(test)]
#[path = "peer_actor/tests.rs"]
mod tests;
