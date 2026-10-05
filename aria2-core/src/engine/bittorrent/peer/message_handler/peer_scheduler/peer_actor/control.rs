//! Swarm-facing controls and lifecycle handles for a long-lived peer actor.
use super::*;

pub(crate) fn local_metadata_response(
    metadata: Option<&[u8]>,
    piece: u32,
) -> aria2_protocol::bittorrent::message::extension::UtMetadataMessage {
    use aria2_protocol::bittorrent::message::extension::UtMetadataMessage;

    metadata
        .and_then(|metadata| {
            let total_size = u32::try_from(metadata.len()).ok()?;
            let start = usize::try_from(piece)
                .ok()?
                .checked_mul(METADATA_PIECE_SIZE)?;
            if start >= metadata.len() {
                return None;
            }
            let end = start
                .saturating_add(METADATA_PIECE_SIZE)
                .min(metadata.len());
            Some(UtMetadataMessage::Data {
                piece,
                total_size,
                data: metadata[start..end].to_vec(),
            })
        })
        .unwrap_or(UtMetadataMessage::Reject { piece })
}

pub(crate) enum PeerCommand {
    RequestMetadata {
        piece: u32,
    },
    ActivatePayload(Arc<PeerActorPayloadConfig>),
    Request {
        generation: RequestGeneration,
        piece_index: u32,
        request: BlockRequest,
    },
    Cancel {
        generation: RequestGeneration,
        piece_index: u32,
        request: BlockRequest,
    },
    HavePiece {
        piece_index: u32,
    },
    SendPex(Vec<u8>),
    AnnounceAvailability,
    Shutdown,
}

pub(crate) struct PeerActorPayloadConfig {
    pub(crate) network_info_hash: [u8; 20],
    pub(crate) local_metadata: Arc<[u8]>,
    pub(crate) piece_length: u32,
    pub(crate) num_pieces: u32,
    pub(crate) total_length: u64,
    pub(crate) upload_config: crate::engine::bittorrent::peer::upload_session::BtSeedingConfig,
    pub(crate) upload_limiter: crate::rate_limiter::RateLimiter,
    pub(crate) auto_unchoke: bool,
    pub(crate) upload_counter: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) upload_progress: Arc<crate::request::request_group::AtomicProgress>,
    pub(crate) provider:
        Arc<dyn crate::engine::bittorrent::peer::upload_session::PieceDataProvider>,
}

pub(crate) enum PeerEvent {
    Message {
        actor_id: PeerActorId,
        generation: RequestGeneration,
        message: aria2_protocol::bittorrent::message::types::BtMessage,
        stats: Option<Box<crate::engine::bittorrent::peer::stats::PeerStats>>,
    },
    InterestChanged {
        actor_id: PeerActorId,
        snapshot: Box<crate::engine::bittorrent::peer::stats::PeerStats>,
    },
    AmInterestChanged {
        actor_id: PeerActorId,
        snapshot: Box<crate::engine::bittorrent::peer::stats::PeerStats>,
    },
    ChokeStateChanged {
        actor_id: PeerActorId,
        snapshot: Box<crate::engine::bittorrent::peer::stats::PeerStats>,
    },
    PeerChokingChanged {
        actor_id: PeerActorId,
        peer_choking: bool,
    },
    AllowedFast {
        actor_id: PeerActorId,
        piece_index: u32,
    },
    PeerAvailabilityChanged {
        actor_id: PeerActorId,
        piece_index: u32,
        has_piece: bool,
    },
    PeerAvailabilitySnapshot {
        actor_id: PeerActorId,
        bitfield: Vec<u8>,
        seeder: bool,
    },
    OutstandingDownloadRequests {
        actor_id: PeerActorId,
        count: usize,
    },
    ExtensionHandshakeReceived {
        actor_id: PeerActorId,
        ut_pex_id: Option<u8>,
        ut_metadata_id: Option<u8>,
        metadata_size: Option<u32>,
        remote_listen_port: Option<u16>,
    },
    MetadataMessage {
        actor_id: PeerActorId,
        message: aria2_protocol::bittorrent::message::extension::UtMetadataMessage,
    },
    PexPeers {
        peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    },
    TrackerPeers {
        peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    },
    UploadBytes {
        actor_id: PeerActorId,
        bytes: u64,
        recorded_at: Instant,
        snapshot: Box<crate::engine::bittorrent::peer::stats::PeerStats>,
    },
    UploadQueueChanged {
        actor_id: PeerActorId,
        snapshot: Box<crate::engine::bittorrent::peer::stats::PeerStats>,
    },
    RequestFailed {
        actor_id: PeerActorId,
        generation: RequestGeneration,
        piece_index: u32,
        request: BlockRequest,
    },
    Disconnected {
        actor_id: PeerActorId,
    },
    GracefulDisconnected {
        actor_id: PeerActorId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PeerGenerationUpdate {
    Begin {
        generation: RequestGeneration,
        piece_index: u32,
    },
    End {
        generation: RequestGeneration,
        piece_index: u32,
    },
}

#[derive(Clone)]
pub(super) struct PeerActorDesiredState {
    pub(super) wanted_pieces: Arc<[u8]>,
    pub(super) choke_upload: bool,
    pub(super) local_seeder: bool,
}

pub(crate) struct PeerActorCommandReceiver {
    pub(super) commands: mpsc::Receiver<PeerCommand>,
    pub(super) generation_updates: watch::Receiver<HashMap<u32, RequestGeneration>>,
    pub(super) desired_state_updates: watch::Receiver<PeerActorDesiredState>,
    pub(super) capacity_updates: watch::Sender<u64>,
}

pub(crate) fn notify_queue_capacity(capacity_updates: &watch::Sender<u64>) {
    capacity_updates.send_modify(|revision| *revision = revision.wrapping_add(1));
}

/// Bounded peer commands plus coalesced snapshots for actor-owned state.
#[derive(Clone)]
pub(crate) struct PeerActorControl {
    commands: mpsc::Sender<PeerCommand>,
    generation_updates: watch::Sender<HashMap<u32, RequestGeneration>>,
    desired_state: watch::Sender<PeerActorDesiredState>,
    capacity_updates: watch::Sender<u64>,
}

impl PeerActorControl {
    #[cfg(test)]
    pub(crate) fn channel(capacity: usize) -> (Self, PeerActorCommandReceiver) {
        Self::channel_with_initial_choke(capacity, true)
    }

    pub(super) fn channel_with_initial_choke(
        capacity: usize,
        choke_upload: bool,
    ) -> (Self, PeerActorCommandReceiver) {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        let (generation_updates, generation_update_receiver) = watch::channel(HashMap::new());
        let (desired_state, desired_state_updates) = watch::channel(PeerActorDesiredState {
            wanted_pieces: Arc::from([]),
            choke_upload,
            local_seeder: false,
        });
        let (capacity_updates, _) = watch::channel(0);
        (
            Self {
                commands: sender,
                generation_updates,
                desired_state,
                capacity_updates: capacity_updates.clone(),
            },
            PeerActorCommandReceiver {
                commands: receiver,
                generation_updates: generation_update_receiver,
                desired_state_updates,
                capacity_updates,
            },
        )
    }

    pub(super) fn queue_capacity_updates(&self) -> watch::Receiver<u64> {
        self.capacity_updates.subscribe()
    }

    pub(crate) async fn send(
        &self,
        command: PeerCommand,
    ) -> std::result::Result<(), mpsc::error::SendError<PeerCommand>> {
        self.commands.send(command).await
    }

    pub(crate) fn try_send(
        &self,
        command: PeerCommand,
    ) -> std::result::Result<(), mpsc::error::TrySendError<PeerCommand>> {
        self.commands.try_send(command)
    }

    pub(crate) fn begin_generation(
        &self,
        generation: RequestGeneration,
        piece_index: u32,
    ) -> std::result::Result<(), PeerGenerationUpdate> {
        self.update_generation(PeerGenerationUpdate::Begin {
            generation,
            piece_index,
        })
    }

    pub(crate) fn try_request(
        &self,
        generation: RequestGeneration,
        piece_index: u32,
        request: BlockRequest,
    ) -> std::result::Result<(), mpsc::error::TrySendError<PeerCommand>> {
        self.try_send(PeerCommand::Request {
            generation,
            piece_index,
            request,
        })
    }

    pub(crate) fn try_cancel(
        &self,
        generation: RequestGeneration,
        piece_index: u32,
        request: BlockRequest,
    ) -> std::result::Result<(), mpsc::error::TrySendError<PeerCommand>> {
        self.try_send(PeerCommand::Cancel {
            generation,
            piece_index,
            request,
        })
    }

    pub(crate) fn end_generation(
        &self,
        generation: RequestGeneration,
        piece_index: u32,
    ) -> std::result::Result<(), PeerGenerationUpdate> {
        self.update_generation(PeerGenerationUpdate::End {
            generation,
            piece_index,
        })
    }

    fn update_generation(
        &self,
        update: PeerGenerationUpdate,
    ) -> std::result::Result<(), PeerGenerationUpdate> {
        if self.generation_updates.receiver_count() == 0 {
            return Err(update);
        }
        self.generation_updates
            .send_if_modified(|active| match update {
                PeerGenerationUpdate::Begin {
                    generation,
                    piece_index,
                } => {
                    if active
                        .get(&piece_index)
                        .is_some_and(|current| current.is_at_least(generation))
                    {
                        return false;
                    }
                    active.insert(piece_index, generation);
                    true
                }
                PeerGenerationUpdate::End {
                    generation,
                    piece_index,
                } => {
                    if active.get(&piece_index) != Some(&generation) {
                        return false;
                    }
                    active.remove(&piece_index);
                    true
                }
            });
        Ok(())
    }

    pub(crate) fn set_wanted_pieces(
        &self,
        wanted_pieces: Arc<[u8]>,
    ) -> std::result::Result<(), watch::error::SendError<Arc<[u8]>>> {
        if self.desired_state.receiver_count() == 0 {
            return Err(watch::error::SendError(wanted_pieces));
        }
        self.desired_state.send_if_modified(|state| {
            if state.wanted_pieces.as_ref() == wanted_pieces.as_ref() {
                return false;
            }
            state.wanted_pieces = wanted_pieces;
            true
        });
        Ok(())
    }

    pub(crate) fn set_upload_choked(&self, choked: bool) -> bool {
        if self.desired_state.receiver_count() == 0 {
            return false;
        }
        self.desired_state.send_if_modified(|state| {
            if state.choke_upload == choked {
                return false;
            }
            state.choke_upload = choked;
            true
        });
        true
    }

    pub(crate) fn desired_upload_choked(&self) -> bool {
        self.desired_state.borrow().choke_upload
    }

    pub(crate) fn set_local_seeder(&self, local_seeder: bool) -> bool {
        if self.desired_state.receiver_count() == 0 {
            return false;
        }
        self.desired_state.send_if_modified(|state| {
            if state.local_seeder == local_seeder {
                return false;
            }
            state.local_seeder = local_seeder;
            true
        });
        true
    }
}

/// Tokio-owned peer actor used when the connection lifetime outlives one
/// piece-transfer future.
pub(crate) struct PeerActorTask {
    pub(crate) control: PeerActorControl,
    pub(super) task: Option<JoinHandle<()>>,
}

impl PeerActorTask {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_owned(
        actor_id: PeerActorId,
        mut connection: BtPeerConn,
        event_tx: mpsc::Sender<PeerEvent>,
        dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        upload_provider: Option<
            Arc<dyn crate::engine::bittorrent::peer::upload_session::PieceDataProvider>,
        >,
        pending_download_requests: Arc<AtomicUsize>,
        command_capacity: usize,
    ) -> Self {
        let initial_choke = connection
            .upload_state
            .as_ref()
            .is_none_or(|state| state.is_peer_choked());
        let (control, command_rx) =
            PeerActorControl::channel_with_initial_choke(command_capacity, initial_choke);
        let task = tokio::spawn(async move {
            run_peer_actor(
                actor_id,
                &mut connection,
                command_rx,
                event_tx,
                dht_engine,
                upload_provider,
                pending_download_requests,
            )
            .await;
        });
        Self {
            control,
            task: Some(task),
        }
    }

    pub(crate) async fn shutdown(&mut self) -> std::result::Result<(), tokio::task::JoinError> {
        let _ = self.control.try_send(PeerCommand::Shutdown);
        let Some(task) = self.task.as_mut() else {
            return Ok(());
        };
        let joined = match tokio::time::timeout(PEER_ACTOR_SHUTDOWN_GRACE, &mut *task).await {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!(
                    "BitTorrent peer actor exceeded its shutdown grace; aborting stalled socket I/O"
                );
                task.abort();
                task.await
            }
        };
        self.task.take();
        match joined {
            Ok(()) => Ok(()),
            Err(error) if error.is_cancelled() => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl Drop for PeerActorTask {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
