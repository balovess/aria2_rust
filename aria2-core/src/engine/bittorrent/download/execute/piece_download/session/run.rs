use std::collections::HashMap;
use std::time::Instant;

use crate::engine::bittorrent::piece::selector::BtPieceSelector;
use crate::engine::work_scheduler::{WorkId, WorkItem, WorkScheduler};
use crate::error::{Aria2Error, Result};
use crate::util::rwlock_ext::RwLockRecover;
use tracing::{debug, warn};

use super::lifecycle::DownloadHalt;
use super::peer_dials::{PeerDialConfig, PeerDialQueue};
use super::wait::{NoPeerWaitEvent, PieceDownloadWait, wait_for_incoming_peer};
use super::web_seed::{MAX_WEB_SEED_PIECES_IN_FLIGHT, wait_for_uri_generation};
use super::work::{requeue_interrupted_piece_work, retry_bt_piece_work};
use super::{PieceDownloadSession, PieceLoopAction, bt_work_scheduler_error};

mod peer_discovery;

impl PieceDownloadSession<'_> {
    pub(super) async fn run_loop(mut self) -> Result<()> {
        let mut peer_dials = PeerDialQueue::default();
        let peer_dial_config = PeerDialConfig::new(
            self.command,
            self.meta.network_info_hash(),
            self.meta.info_hash_v2,
            self.num_pieces,
            self.piece_length,
            self.total_size,
        );
        let mut web_seed_tasks = tokio::task::JoinSet::new();
        let mut active_web_seed_work = HashMap::new();
        let mut web_seed_work_queue = WorkScheduler::new();
        let mut web_seed_scan_cursor = 0u32;
        let mut observed_uri_generation = self.command.group.recover().uri_generation();
        let web_seed_concurrency =
            self.command
                .group
                .recover()
                .options()
                .split
                .unwrap_or(crate::constants::DEFAULT_SPLIT)
                .clamp(1, MAX_WEB_SEED_PIECES_IN_FLIGHT as u64) as usize;
        let mut piece_work_queue = WorkScheduler::new();
        self.announce_available_pieces().await;
        self.apply_upload_choke_round();
        loop {
            if let Some(result) = peer_dials.try_join_next() {
                self.handle_peer_dial_batch_result(result).await;
            }
            self.start_next_peer_dial_batch(&mut peer_dials, &peer_dial_config);
            self.refresh_upload_stats();
            if !self.swarm.is_empty()
                && self
                    .command
                    .choking_algo
                    .as_ref()
                    .is_some_and(|algo| algo.choke_rotation_due(Instant::now()))
            {
                self.apply_upload_choke_round();
            }
            self.command
                .drain_incoming_peers_to_swarm(self.swarm, self.peer_actor_admission_context())
                .await;
            self.command.bt_runtime.set_connections(self.swarm.len());

            match self.check_download_halt() {
                DownloadHalt::Continue => {}
                DownloadHalt::Timeout => continue,
                DownloadHalt::Requested => {
                    return self
                        .finish_halted_download(
                            &mut piece_work_queue,
                            &mut web_seed_work_queue,
                            &mut web_seed_tasks,
                        )
                        .await;
                }
            }

            // Tracker results use the same Swarm event stream even while there
            // is no connected peer or a piece batch is waiting on a response.
            // Admit them at the next coordinator turn before entering an idle
            // wait, so an empty swarm cannot strand discovered endpoints.
            self.admit_pending_tracker_peers(&mut peer_dials, &peer_dial_config);

            let current_uri_generation = self.command.group.recover().uri_generation();
            if current_uri_generation != observed_uri_generation {
                observed_uri_generation = current_uri_generation;
                web_seed_scan_cursor = 0;
                web_seed_work_queue.discard_pending();
            }
            let mut joined_web_seed_task = false;
            while let Some(joined) = web_seed_tasks.try_join_next() {
                match joined {
                    Ok((piece_index, result)) => {
                        joined_web_seed_task = true;
                        let lease = active_web_seed_work.remove(&piece_index).ok_or_else(|| {
                            bt_work_scheduler_error(
                                "find a completed WebSeed lease",
                                format!("no active lease for piece {piece_index}"),
                            )
                        })?;
                        self.complete_web_seed_task(
                            piece_index,
                            result,
                            lease,
                            &mut web_seed_work_queue,
                            &mut piece_work_queue,
                        )
                        .await?;
                    }
                    Err(error) => {
                        joined_web_seed_task = true;
                        warn!(%error, "WebSeed worker terminated unexpectedly");
                        self.abort_web_seed_tasks(
                            &mut web_seed_tasks,
                            &mut active_web_seed_work,
                            &mut web_seed_work_queue,
                        )?;
                        break;
                    }
                }
            }

            // Replenish after harvesting completed tasks so a full batch that
            // finishes together cannot leave queued WebSeed work idle.
            self.schedule_web_seed_pieces(
                &mut web_seed_tasks,
                &mut active_web_seed_work,
                &mut web_seed_scan_cursor,
                &mut web_seed_work_queue,
                web_seed_concurrency,
            )?;

            if BtPieceSelector::is_complete(&self.piece_picker) {
                if self.endgame_state.is_endgame_active() {
                    self.endgame_state.exit_endgame();
                }
                self.swarm.set_local_seeder(true);
                break;
            }

            // Refill the WebSeed pipeline before an idle peer wait. The
            // completion-drain above can empty the JoinSet after this loop's
            // initial scheduling pass, while scan_cursor still has pieces
            // that have never been requested.
            if joined_web_seed_task {
                continue;
            }

            self.update_peer_discovery(&mut peer_dials, &peer_dial_config)
                .await;

            // With no connected peers, keep the torrent alive for tracker,
            // DHT, PEX, or incoming-peer discovery. The wait is driven by a
            // socket/message event, a lifecycle notification, a completed DHT
            // lookup, or the next protocol/stop-timeout deadline.
            if self.swarm.is_empty() {
                debug!("[BT] No peers available, waiting for peer discovery...");
                let peer_deadline = self
                    .command
                    .next_peer_event_deadline(self.swarm.len(), self.stop_timeout.deadline());
                let protocol_deadline = web_seed_work_queue
                    .next_retry_deadline()
                    .map_or(peer_deadline, |retry_deadline| {
                        peer_deadline.min(retry_deadline)
                    });
                let deadline = self
                    .next_snub_check_deadline()
                    .map_or(protocol_deadline, |snub_deadline| {
                        protocol_deadline.min(snub_deadline)
                    });
                let (uri_generation, uri_notifier) = {
                    let group = self.command.group.recover();
                    (group.uri_generation_handle(), group.uri_notifier())
                };
                let peer_event = self.command.wait_for_swarm_peer_event(self.swarm, deadline);
                let event = tokio::select! {
                    event = peer_event => NoPeerWaitEvent::Peer(event),
                    joined = peer_dials.join_next(), if peer_dials.has_active_batch() => {
                        NoPeerWaitEvent::PeerDial(joined)
                    }
                    joined = web_seed_tasks.join_next(), if !web_seed_tasks.is_empty() => {
                        NoPeerWaitEvent::WebSeed(joined)
                    }
                    _ = wait_for_uri_generation(
                        uri_generation,
                        uri_notifier,
                        observed_uri_generation,
                    ) => NoPeerWaitEvent::UriChanged,
                };
                let event = match event {
                    NoPeerWaitEvent::WebSeed(Some(Ok((piece_index, result)))) => {
                        let lease = active_web_seed_work.remove(&piece_index).ok_or_else(|| {
                            bt_work_scheduler_error(
                                "find a completed WebSeed lease",
                                format!("no active lease for piece {piece_index}"),
                            )
                        })?;
                        self.complete_web_seed_task(
                            piece_index,
                            result,
                            lease,
                            &mut web_seed_work_queue,
                            &mut piece_work_queue,
                        )
                        .await?;
                        continue;
                    }
                    NoPeerWaitEvent::WebSeed(Some(Err(error))) => {
                        warn!(%error, "WebSeed worker terminated unexpectedly");
                        self.abort_web_seed_tasks(
                            &mut web_seed_tasks,
                            &mut active_web_seed_work,
                            &mut web_seed_work_queue,
                        )?;
                        continue;
                    }
                    NoPeerWaitEvent::PeerDial(Some(result)) => {
                        self.handle_peer_dial_batch_result(result).await;
                        self.start_next_peer_dial_batch(&mut peer_dials, &peer_dial_config);
                        continue;
                    }
                    NoPeerWaitEvent::PeerDial(None) => continue,
                    NoPeerWaitEvent::WebSeed(None) | NoPeerWaitEvent::UriChanged => continue,
                    NoPeerWaitEvent::Peer(event) => event,
                };
                self.handle_peer_wait_event(event).await;
                continue;
            }

            let mut first_lease = piece_work_queue.admit_one_where(
                super::piece_batch::MAX_BT_PIECES_IN_FLIGHT,
                Instant::now(),
                |piece_index| self.piece_picker.is_selectable(*piece_index),
            );
            let next_piece_idx = if let Some(lease) = first_lease.as_ref() {
                *lease.payload() as usize
            } else {
                let mut source_candidate = None;
                for _ in 0..self.num_pieces {
                    let remaining = self.piece_picker.remaining_count();
                    let Some(candidate) = self
                        .piece_selector
                        .select_next_piece(&mut self.piece_picker, remaining)
                        .piece_index
                    else {
                        break;
                    };
                    let work_id = WorkId::new(candidate as u64);
                    if !piece_work_queue.is_scheduled(work_id) {
                        source_candidate = Some(candidate);
                        break;
                    }
                }
                match source_candidate {
                    Some(idx) => idx,
                    None => {
                        if !web_seed_tasks.is_empty() {
                            let peer_deadline = self.command.next_peer_event_deadline(
                                self.swarm.len(),
                                self.stop_timeout.deadline(),
                            );
                            let protocol_deadline = web_seed_work_queue
                                .next_retry_deadline()
                                .map_or(peer_deadline, |retry_deadline| {
                                    peer_deadline.min(retry_deadline)
                                });
                            let choke_deadline = (!self.swarm.is_empty())
                                .then(|| {
                                    self.command
                                        .choking_algo
                                        .as_ref()
                                        .and_then(|algo| algo.next_choke_rotation_deadline())
                                })
                                .flatten();
                            let deadline = choke_deadline
                                .map_or(protocol_deadline, |choke_deadline| {
                                    protocol_deadline.min(choke_deadline)
                                });
                            let deadline = self
                                .next_snub_check_deadline()
                                .map_or(deadline, |snub_deadline| deadline.min(snub_deadline));
                            let (uri_generation, uri_notifier) = {
                                let group = self.command.group.recover();
                                (group.uri_generation_handle(), group.uri_notifier())
                            };
                            let peer_event =
                                self.command.wait_for_swarm_peer_event(self.swarm, deadline);
                            let uri_changed = wait_for_uri_generation(
                                uri_generation,
                                uri_notifier,
                                observed_uri_generation,
                            );
                            let event = tokio::select! {
                                event = peer_event => NoPeerWaitEvent::Peer(event),
                                joined = web_seed_tasks.join_next() => NoPeerWaitEvent::WebSeed(joined),
                                joined = peer_dials.join_next(), if peer_dials.has_active_batch() => {
                                    NoPeerWaitEvent::PeerDial(joined)
                                },
                                _ = uri_changed => NoPeerWaitEvent::UriChanged,
                            };
                            match event {
                                NoPeerWaitEvent::WebSeed(Some(Ok((piece_index, result)))) => {
                                    let lease = active_web_seed_work
                                        .remove(&piece_index)
                                        .ok_or_else(|| {
                                            bt_work_scheduler_error(
                                                "find a completed WebSeed lease",
                                                format!("no active lease for piece {piece_index}"),
                                            )
                                        })?;
                                    self.complete_web_seed_task(
                                        piece_index,
                                        result,
                                        lease,
                                        &mut web_seed_work_queue,
                                        &mut piece_work_queue,
                                    )
                                    .await?;
                                }
                                NoPeerWaitEvent::WebSeed(Some(Err(error))) => {
                                    warn!(%error, "WebSeed worker terminated unexpectedly");
                                    self.abort_web_seed_tasks(
                                        &mut web_seed_tasks,
                                        &mut active_web_seed_work,
                                        &mut web_seed_work_queue,
                                    )?;
                                }
                                NoPeerWaitEvent::WebSeed(None) => {}
                                NoPeerWaitEvent::PeerDial(Some(result)) => {
                                    self.handle_peer_dial_batch_result(result).await;
                                    self.start_next_peer_dial_batch(
                                        &mut peer_dials,
                                        &peer_dial_config,
                                    );
                                }
                                NoPeerWaitEvent::PeerDial(None) | NoPeerWaitEvent::UriChanged => {}
                                NoPeerWaitEvent::Peer(event) => {
                                    self.handle_peer_wait_event(event).await;
                                }
                            }
                            continue;
                        }
                        tracing::debug!("[BT] No piece available, waiting...");
                        let peer_deadline = self.command.next_peer_event_deadline(
                            self.swarm.len(),
                            self.stop_timeout.deadline(),
                        );
                        let protocol_deadline = web_seed_work_queue
                            .next_retry_deadline()
                            .map_or(peer_deadline, |retry_deadline| {
                                peer_deadline.min(retry_deadline)
                            });
                        let choke_deadline = (!self.swarm.is_empty())
                            .then(|| {
                                self.command
                                    .choking_algo
                                    .as_ref()
                                    .and_then(|algo| algo.next_choke_rotation_deadline())
                            })
                            .flatten();
                        let deadline = choke_deadline.map_or(protocol_deadline, |choke_deadline| {
                            protocol_deadline.min(choke_deadline)
                        });
                        let deadline = self
                            .next_snub_check_deadline()
                            .map_or(deadline, |snub_deadline| deadline.min(snub_deadline));
                        let peer_event =
                            self.command.wait_for_swarm_peer_event(self.swarm, deadline);
                        let event = tokio::select! {
                            event = peer_event => NoPeerWaitEvent::Peer(event),
                            joined = peer_dials.join_next(), if peer_dials.has_active_batch() => {
                                NoPeerWaitEvent::PeerDial(joined)
                            }
                        };
                        let event = match event {
                            NoPeerWaitEvent::PeerDial(Some(result)) => {
                                self.handle_peer_dial_batch_result(result).await;
                                self.start_next_peer_dial_batch(&mut peer_dials, &peer_dial_config);
                                continue;
                            }
                            NoPeerWaitEvent::PeerDial(None) => continue,
                            NoPeerWaitEvent::Peer(event) => event,
                            NoPeerWaitEvent::WebSeed(_) | NoPeerWaitEvent::UriChanged => continue,
                        };
                        self.handle_peer_wait_event(event).await;
                        continue;
                    }
                }
            };

            if first_lease.is_none() {
                let piece_index = u32::try_from(next_piece_idx).map_err(|_| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(
                        "BitTorrent piece index exceeds the work scheduler limit".into(),
                    ))
                })?;
                let work_id = WorkId::new(u64::from(piece_index));
                piece_work_queue
                    .enqueue(WorkItem::new(work_id, piece_index, 0))
                    .map_err(|error| bt_work_scheduler_error("queue a selected piece", error))?;
                first_lease = piece_work_queue.admit_one_where(
                    super::piece_batch::MAX_BT_PIECES_IN_FLIGHT,
                    Instant::now(),
                    |candidate| self.piece_picker.is_selectable(*candidate),
                );
            }
            let first_lease = first_lease.ok_or_else(|| {
                Aria2Error::Fatal(crate::error::FatalError::Config(
                    "BitTorrent selected a piece that the core scheduler could not admit".into(),
                ))
            })?;
            let next_piece_idx = *first_lease.payload() as usize;

            let batch_limit = self.normal_piece_batch_limit(next_piece_idx);
            let mut selected_leases = vec![first_lease];
            let mut selected_pieces = vec![next_piece_idx];
            self.piece_picker
                .mark_in_progress(next_piece_idx as u32, true);
            while selected_leases.len() < batch_limit {
                if let Some(lease) =
                    piece_work_queue.admit_one_where(batch_limit, Instant::now(), |piece_index| {
                        self.piece_picker.is_selectable(*piece_index)
                    })
                {
                    let piece_index = *lease.payload();
                    self.piece_picker.mark_in_progress(piece_index, true);
                    selected_pieces.push(piece_index as usize);
                    selected_leases.push(lease);
                    continue;
                }

                let mut source_candidate = None;
                for _ in 0..self.num_pieces {
                    let remaining = self.piece_picker.remaining_count();
                    let Some(candidate) = self
                        .piece_selector
                        .select_next_piece(&mut self.piece_picker, remaining)
                        .piece_index
                    else {
                        break;
                    };
                    if selected_pieces.contains(&candidate) {
                        break;
                    }
                    if piece_work_queue.is_scheduled(WorkId::new(candidate as u64)) {
                        continue;
                    }
                    source_candidate = Some(candidate);
                    break;
                }
                let Some(candidate) = source_candidate else {
                    break;
                };
                let piece_index = u32::try_from(candidate).map_err(|_| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(
                        "BitTorrent piece index exceeds the work scheduler limit".into(),
                    ))
                })?;
                piece_work_queue
                    .enqueue(WorkItem::new(
                        WorkId::new(u64::from(piece_index)),
                        piece_index,
                        0,
                    ))
                    .map_err(|error| bt_work_scheduler_error("queue a selected piece", error))?;
                let Some(lease) =
                    piece_work_queue.admit_one_where(batch_limit, Instant::now(), |candidate| {
                        self.piece_picker.is_selectable(*candidate)
                    })
                else {
                    return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                        "BitTorrent selected a piece that the core scheduler could not admit"
                            .into(),
                    )));
                };
                self.piece_picker.mark_in_progress(piece_index, true);
                selected_pieces.push(piece_index as usize);
                selected_leases.push(lease);
            }
            let mut active_work = selected_leases
                .into_iter()
                .map(|lease| (*lease.payload(), lease))
                .collect::<HashMap<_, _>>();
            let incoming_receiver = self.command.incoming_peers.clone();
            let wait = if let Some(deadline) = self.stop_timeout.deadline() {
                tokio::select! {
                    actions = self.download_piece_batch(&selected_pieces) => PieceDownloadWait::Completed(actions?),
                    incoming = wait_for_incoming_peer(incoming_receiver) => PieceDownloadWait::Incoming(incoming.map(Box::new)),
                    joined = peer_dials.join_next(), if peer_dials.has_active_batch() => {
                        PieceDownloadWait::PeerDial(joined)
                    }
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                        PieceDownloadWait::StopTimeout
                    }
                }
            } else {
                tokio::select! {
                    actions = self.download_piece_batch(&selected_pieces) => PieceDownloadWait::Completed(actions?),
                    incoming = wait_for_incoming_peer(incoming_receiver) => PieceDownloadWait::Incoming(incoming.map(Box::new)),
                    joined = peer_dials.join_next(), if peer_dials.has_active_batch() => {
                        PieceDownloadWait::PeerDial(joined)
                    }
                }
            };
            let action = match wait {
                PieceDownloadWait::Completed(action) => action,
                PieceDownloadWait::Incoming(Some(incoming)) => {
                    let context = self.peer_actor_admission_context();
                    self.command
                        .admit_incoming_peer_to_swarm(self.swarm, *incoming, &context);
                    self.announce_available_pieces().await;
                    requeue_interrupted_piece_work(
                        &mut piece_work_queue,
                        &mut self.piece_picker,
                        &mut active_work,
                    )?;
                    continue;
                }
                PieceDownloadWait::Incoming(None) => {
                    self.command.incoming_peers = None;
                    requeue_interrupted_piece_work(
                        &mut piece_work_queue,
                        &mut self.piece_picker,
                        &mut active_work,
                    )?;
                    continue;
                }
                PieceDownloadWait::PeerDial(Some(result)) => {
                    self.handle_peer_dial_batch_result(result).await;
                    self.start_next_peer_dial_batch(&mut peer_dials, &peer_dial_config);
                    requeue_interrupted_piece_work(
                        &mut piece_work_queue,
                        &mut self.piece_picker,
                        &mut active_work,
                    )?;
                    continue;
                }
                PieceDownloadWait::PeerDial(None) => {
                    requeue_interrupted_piece_work(
                        &mut piece_work_queue,
                        &mut self.piece_picker,
                        &mut active_work,
                    )?;
                    continue;
                }
                PieceDownloadWait::StopTimeout => {
                    requeue_interrupted_piece_work(
                        &mut piece_work_queue,
                        &mut self.piece_picker,
                        &mut active_work,
                    )?;
                    continue;
                }
            };
            let mut refresh_progress = false;
            for (piece_index, action) in action {
                let piece_index = u32::try_from(piece_index).map_err(|_| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(
                        "BitTorrent result piece index exceeds the work scheduler limit".into(),
                    ))
                })?;
                let lease = active_work.remove(&piece_index).ok_or_else(|| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "BitTorrent work scheduler has no active lease for piece {piece_index}"
                    )))
                })?;
                match action {
                    PieceLoopAction::Retry => {
                        self.piece_picker.mark_in_progress(piece_index, false);
                        retry_bt_piece_work(&mut piece_work_queue, lease)?;
                    }
                    PieceLoopAction::RefreshProgress => {
                        piece_work_queue
                            .complete(lease)
                            .map_err(|error| bt_work_scheduler_error("complete a piece", error))?;
                        refresh_progress = true;
                    }
                }
            }
            requeue_interrupted_piece_work(
                &mut piece_work_queue,
                &mut self.piece_picker,
                &mut active_work,
            )?;
            if refresh_progress {
                self.refresh_download_progress();
            }
        }
        self.finalize_completed_download().await
    }
}
