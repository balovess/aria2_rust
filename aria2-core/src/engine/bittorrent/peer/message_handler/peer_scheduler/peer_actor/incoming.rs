//! Decodes actor-owned peer messages and publishes their swarm effects.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IncomingMessageAction {
    Continue,
    Exit,
}

pub(super) struct IncomingMessageContext<'a> {
    pub(super) actor_id: PeerActorId,
    pub(super) connection: &'a mut BtPeerConn,
    pub(super) event_tx: &'a mpsc::Sender<PeerEvent>,
    pub(super) dht_engine: Option<&'a Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    pub(super) upload_provider:
        Option<&'a Arc<dyn crate::engine::bittorrent::peer::upload_session::PieceDataProvider>>,
    pub(super) requests: &'a mut PeerRequestLedger,
    pub(super) local_seeder: bool,
    pub(super) local_ut_metadata_id: u8,
    pub(super) wanted_pieces: &'a [u8],
    pub(super) shutdown_requested: bool,
    pub(super) upload_flush_deadline: &'a mut Option<tokio::time::Instant>,
}

impl IncomingMessageContext<'_> {
    pub(super) async fn handle(
        self,
        message: Result<Option<aria2_protocol::bittorrent::message::types::BtMessage>>,
    ) -> IncomingMessageAction {
        let Self {
            actor_id,
            connection,
            event_tx,
            dht_engine,
            upload_provider,
            requests,
            local_seeder,
            local_ut_metadata_id,
            wanted_pieces,
            shutdown_requested,
            upload_flush_deadline,
        } = self;
        let connection = &mut *connection;
        let requests = &mut *requests;
        let dht_engine = dht_engine.map(Arc::clone);
        let upload_provider = upload_provider.map(|provider| provider.as_ref());

        match message {
            Ok(Some(message)) => {
                use aria2_protocol::bittorrent::message::types::BtMessage;
                let remote_extension_handshake = match &message {
                    BtMessage::Extended {
                        ext_id: 0,
                        payload,
                    } => aria2_protocol::bittorrent::message::extension::
                        ExtensionHandshake::from_bytes(payload)
                        .ok(),
                    _ => None,
                };
                let metadata_message = match &message {
                    BtMessage::Extended { ext_id, payload }
                        if *ext_id == local_ut_metadata_id =>
                    {
                        match aria2_protocol::bittorrent::message::extension::
                            UtMetadataMessage::from_payload(payload)
                        {
                            Ok(parsed) => Some(parsed),
                            Err(error) => {
                                tracing::debug!(actor_id = actor_id.0, %error, "Invalid BEP 9 metadata message");
                                let _ = event_tx
                                    .send(PeerEvent::Disconnected { actor_id })
                                    .await;
                                return IncomingMessageAction::Exit;
                            }
                        }
                    }
                    _ => None,
                };
                let was_interested = connection.stats.peer_interested;
                let was_peer_choking = connection.stats.peer_choking;
                let was_seeder = connection.seeder;
                let outstanding_upload_count = connection.stats.outstanding_upload_count;
                let peer_availability_change =
                    match &message {
                        aria2_protocol::bittorrent::message::types::BtMessage::Have {
                            piece_index,
                        } if !connection.is_metadata_pending() => Some(*piece_index),
                        _ => None,
                    };
                let full_availability_change = !connection.is_metadata_pending()
                    && matches!(
                        message,
                        aria2_protocol::bittorrent::message::types::BtMessage::Bitfield { .. }
                            | aria2_protocol::bittorrent::message::types::BtMessage::HaveAll
                            | aria2_protocol::bittorrent::message::types::BtMessage::HaveNone
                    );
                let received_extension_handshake = matches!(
                    &message,
                    aria2_protocol::bittorrent::message::types::BtMessage::Extended {
                        ext_id: 0,
                        ..
                    }
                );
                let allowed_fast_piece = match &message {
                    aria2_protocol::bittorrent::message::types::BtMessage::AllowedFast {
                        index,
                    } if !connection.is_metadata_pending() => Some(*index),
                    _ => None,
                };
                let (message, uploaded_bytes) = match process_peer_message(
                    connection,
                    message,
                    dht_engine.clone(),
                    upload_provider,
                )
                .await
                {
                    Ok(message) => message,
                    Err(error) => {
                        tracing::debug!(actor_id = actor_id.0, %error, "BT upload request handling failed");
                        let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                        return IncomingMessageAction::Exit;
                    }
                };
                if local_seeder && connection.is_seeder() {
                    if let Some(resource) = connection.session_resource.as_ref()
                        && event_tx
                            .send(PeerEvent::PeerAvailabilitySnapshot {
                                actor_id,
                                bitfield: resource.bitfield().to_vec(),
                                seeder: true,
                            })
                            .await
                            .is_err()
                    {
                        return IncomingMessageAction::Exit;
                    }
                    tracing::debug!(
                        actor_id = actor_id.0,
                        "Closing BT connection between seeders"
                    );
                    let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                    return IncomingMessageAction::Exit;
                }
                if let Some(piece_index) = allowed_fast_piece
                    && event_tx
                        .send(PeerEvent::AllowedFast {
                            actor_id,
                            piece_index,
                        })
                        .await
                        .is_err()
                {
                    return IncomingMessageAction::Exit;
                }
                if received_extension_handshake
                    && event_tx
                        .send(PeerEvent::ExtensionHandshakeReceived {
                            actor_id,
                            ut_pex_id: connection.peer_extension_id("ut_pex"),
                            // The connection owns the effective BEP 10 map: omitted
                            // entries retain their previous ID, while an explicit zero
                            // removes the mapping. Publish that resolved state so swarm
                            // coordinators can observe capability revocations.
                            ut_metadata_id: connection.peer_extension_id("ut_metadata"),
                            metadata_size: remote_extension_handshake
                                .as_ref()
                                .and_then(|handshake| handshake.metadata_size())
                                .filter(|size| *size > 0),
                            remote_listen_port: connection.remote_listen_port,
                        })
                        .await
                        .is_err()
                {
                    return IncomingMessageAction::Exit;
                }
                if let Some(metadata_message) = metadata_message {
                    use aria2_protocol::bittorrent::message::extension::UtMetadataMessage;
                    match metadata_message {
                        UtMetadataMessage::Request { piece } => {
                            if let Some(ext_id) = connection.peer_extension_id("ut_metadata") {
                                let response = local_metadata_response(
                                    connection.local_metadata.as_deref(),
                                    piece,
                                );
                                let message = BtMessage::Extended {
                                    ext_id,
                                    payload: response.to_payload(),
                                };
                                if connection.send_bt_message(&message).await.is_err() {
                                    let _ =
                                        event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                                    return IncomingMessageAction::Exit;
                                }
                                connection.record_outbound_activity();
                            }
                        }
                        metadata_message => {
                            if connection.is_metadata_pending()
                                && event_tx
                                    .send(PeerEvent::MetadataMessage {
                                        actor_id,
                                        message: metadata_message,
                                    })
                                    .await
                                    .is_err()
                            {
                                return IncomingMessageAction::Exit;
                            }
                        }
                    }
                }
                let discovered_pex_peers = connection.drain_pex_peers();
                if !discovered_pex_peers.is_empty()
                    && event_tx
                        .send(PeerEvent::PexPeers {
                            peers: discovered_pex_peers,
                        })
                        .await
                        .is_err()
                {
                    return IncomingMessageAction::Exit;
                }
                if uploaded_bytes > 0 {
                    connection.record_outbound_activity();
                    let Ok(permit) = event_tx.reserve().await else {
                        return IncomingMessageAction::Exit;
                    };
                    let recorded_at = Instant::now();
                    permit.send(PeerEvent::UploadBytes {
                        actor_id,
                        bytes: uploaded_bytes,
                        recorded_at,
                        snapshot: Box::new(connection.stats.clone()),
                    });
                } else if connection.stats.outstanding_upload_count != outstanding_upload_count
                    && event_tx
                        .send(PeerEvent::UploadQueueChanged {
                            actor_id,
                            snapshot: Box::new(connection.stats.clone()),
                        })
                        .await
                        .is_err()
                {
                    tracing::debug!(
                        actor_id = actor_id.0,
                        "Peer swarm event receiver closed while publishing queued upload state"
                    );
                    return IncomingMessageAction::Exit;
                }
                if connection.stats.outstanding_upload_count != outstanding_upload_count
                    && connection.has_pending_upload_messages()
                {
                    let delay = connection
                        .pending_upload_flush_delay()
                        .unwrap_or(Duration::ZERO);
                    let delay = if delay.is_zero() {
                        Duration::from_millis(2)
                    } else {
                        delay
                    };
                    *upload_flush_deadline = Some(tokio::time::Instant::now() + delay);
                }
                if connection.stats.peer_interested != was_interested
                    && event_tx
                        .send(PeerEvent::InterestChanged {
                            actor_id,
                            snapshot: Box::new(connection.stats.clone()),
                        })
                        .await
                        .is_err()
                {
                    return IncomingMessageAction::Exit;
                }
                if connection.stats.peer_choking != was_peer_choking
                    && event_tx
                        .send(PeerEvent::PeerChokingChanged {
                            actor_id,
                            peer_choking: connection.stats.peer_choking,
                        })
                        .await
                        .is_err()
                {
                    return IncomingMessageAction::Exit;
                }
                if let Some(piece_index) = peer_availability_change
                    && event_tx
                        .send(PeerEvent::PeerAvailabilityChanged {
                            actor_id,
                            piece_index,
                            has_piece: connection.seeder
                                || connection.has_piece(piece_index as usize),
                        })
                        .await
                        .is_err()
                {
                    return IncomingMessageAction::Exit;
                }
                if (full_availability_change || connection.seeder != was_seeder)
                    && let Some(resource) = connection.session_resource.as_ref()
                    && event_tx
                        .send(PeerEvent::PeerAvailabilitySnapshot {
                            actor_id,
                            bitfield: resource.bitfield().to_vec(),
                            seeder: connection.seeder,
                        })
                        .await
                        .is_err()
                {
                    return IncomingMessageAction::Exit;
                }
                if peer_availability_change.is_some() || full_availability_change {
                    match reconcile_peer_interest(actor_id, connection, wanted_pieces, event_tx)
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => return IncomingMessageAction::Exit,
                        Err(error) => {
                            tracing::debug!(actor_id = actor_id.0, %error, "Failed to update BT peer interest");
                            let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                            return IncomingMessageAction::Exit;
                        }
                    }
                }
                let Some(message) = message else {
                    if shutdown_requested && !connection.has_pending_upload_messages() {
                        return IncomingMessageAction::Exit;
                    }
                    return IncomingMessageAction::Continue;
                };
                let generation = match &message {
                    BtMessage::Piece { index, begin, .. }
                    | BtMessage::Reject {
                        index,
                        offset: begin,
                        ..
                    } => requests.complete(*index, *begin),
                    _ => None,
                };
                let Some(generation) = generation else {
                    match &message {
                        BtMessage::Piece { index, begin, .. }
                        | BtMessage::Reject {
                            index,
                            offset: begin,
                            ..
                        } => {
                            trace!(
                                actor_id = actor_id.0,
                                piece_index = index,
                                offset = begin,
                                "Ignoring unsolicited or stale BT block response"
                            );
                        }
                        _ => unreachable!("peer actor only forwards block responses"),
                    }
                    return IncomingMessageAction::Continue;
                };
                let stats = matches!(message, BtMessage::Piece { .. })
                    .then(|| Box::new(connection.stats.clone()));
                if event_tx
                    .send(PeerEvent::Message {
                        actor_id,
                        generation,
                        message,
                        stats,
                    })
                    .await
                    .is_err()
                {
                    return IncomingMessageAction::Exit;
                }
                if shutdown_requested && !connection.has_pending_upload_messages() {
                    IncomingMessageAction::Exit
                } else {
                    IncomingMessageAction::Continue
                }
            }
            Ok(None) => {
                tracing::debug!(actor_id = actor_id.0, "BT peer closed its connection");
                let _ = event_tx
                    .send(PeerEvent::GracefulDisconnected { actor_id })
                    .await;
                IncomingMessageAction::Exit
            }
            Err(error) => {
                tracing::debug!(actor_id = actor_id.0, %error, "BT peer message read failed");
                let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                IncomingMessageAction::Exit
            }
        }
    }
}
