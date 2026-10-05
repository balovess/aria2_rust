//! Peer message side effects and scheduler-facing choke coordination.
use super::*;

pub(super) async fn wait_for_upload_rate_change(
    receivers: &mut Option<(watch::Receiver<u64>, Option<watch::Receiver<u64>>)>,
) {
    let Some((local, global)) = receivers.as_mut() else {
        std::future::pending::<()>().await;
        return;
    };

    if let Some(global) = global {
        tokio::select! {
            _ = local.changed() => {}
            _ = global.changed() => {}
        }
    } else {
        let _ = local.changed().await;
    }
}

pub(super) async fn reconcile_peer_interest(
    actor_id: PeerActorId,
    connection: &mut BtPeerConn,
    wanted_pieces: &[u8],
    event_tx: &mpsc::Sender<PeerEvent>,
) -> Result<bool> {
    let has_wanted_pieces = wanted_pieces.iter().any(|pieces| *pieces != 0);
    let interested = has_wanted_pieces
        && (connection.seeder
            || connection
                .session_resource
                .as_ref()
                .is_some_and(|resource| {
                    wanted_pieces
                        .iter()
                        .zip(resource.bitfield())
                        .any(|(wanted, available)| wanted & available != 0)
                }));
    if connection.stats.am_interested == interested {
        return Ok(true);
    }

    if interested {
        connection.send_interested().await?;
    } else {
        connection.send_not_interested().await?;
    }
    connection.record_outbound_activity();
    Ok(event_tx
        .send(PeerEvent::AmInterestChanged {
            actor_id,
            snapshot: Box::new(connection.stats.clone()),
        })
        .await
        .is_ok())
}

pub(super) async fn reconcile_peer_choke_state(
    actor_id: PeerActorId,
    connection: &mut BtPeerConn,
    should_choke: bool,
    event_tx: &mpsc::Sender<PeerEvent>,
) -> Result<bool> {
    let Some(was_choked) = connection
        .upload_state
        .as_ref()
        .map(|state| state.is_peer_choked())
    else {
        return Ok(true);
    };
    if was_choked == should_choke {
        return Ok(true);
    }

    if should_choke {
        connection.choke_upload_peer().await?;
    } else {
        connection.unchoke_upload_peer().await?;
    }
    connection.record_outbound_activity();
    Ok(event_tx
        .send(PeerEvent::ChokeStateChanged {
            actor_id,
            snapshot: Box::new(connection.stats.clone()),
        })
        .await
        .is_ok())
}

pub(crate) async fn process_peer_message(
    connection: &mut BtPeerConn,
    message: aria2_protocol::bittorrent::message::types::BtMessage,
    dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    upload_provider: Option<
        &dyn crate::engine::bittorrent::peer::upload_session::PieceDataProvider,
    >,
) -> Result<(
    Option<aria2_protocol::bittorrent::message::types::BtMessage>,
    u64,
)> {
    use aria2_protocol::bittorrent::message::types::BtMessage;

    connection.apply_peer_state_message(&message);

    let mut uploaded_bytes = 0;
    if !matches!(message, BtMessage::Piece { .. } | BtMessage::Reject { .. })
        && let Some(provider) = upload_provider
        && connection.upload_state.is_some()
    {
        uploaded_bytes = connection
            .handle_upload_message(message.clone(), provider)
            .await?;
    }

    match message {
        BtMessage::Piece { .. } | BtMessage::Reject { .. } => Ok((Some(message), uploaded_bytes)),
        BtMessage::Port { port } => {
            if port != 0
                && let Ok(ip) = connection.ip_addr.parse()
                && let Some(engine) = dht_engine
            {
                let address = SocketAddr::new(ip, port);
                trace!(peer = %address, "Received DHT port during pipelined block read");
                tokio::spawn(async move {
                    engine.add_node(address).await;
                });
            }
            Ok((None, uploaded_bytes))
        }
        BtMessage::Extended { ext_id, payload } => {
            if connection.is_pex_enabled()
                && ext_id != 0
                && connection.peer_extension_id("ut_pex") == Some(ext_id)
            {
                process_pex_during_read(connection, ext_id, &payload);
            }
            Ok((None, uploaded_bytes))
        }
        _ => Ok((None, uploaded_bytes)),
    }
}

pub(crate) fn apply_interest_change(
    workers: &mut PeerGeneration,
    peers: &PeerSchedulingSnapshot,
    choking_algo: Option<&mut ChokingAlgorithm>,
    snapshot: crate::engine::bittorrent::peer::stats::PeerStats,
) {
    let Some(algo) = choking_algo else { return };
    algo.sync_peer_by_identity(&snapshot);
    apply_choke_round(workers, peers, Some(algo));
}

pub(crate) fn rebalance_upload_slots_after_peer_disconnect(
    workers: &mut PeerGeneration,
    peers: &PeerSchedulingSnapshot,
    choking_algo: Option<&mut ChokingAlgorithm>,
    identity: PeerIdentity,
) {
    let Some(algo) = choking_algo else { return };
    let released_upload_slot = algo.peers().iter().any(|peer| {
        PeerIdentity::from(peer) == identity && peer.peer_interested && !peer.am_choking
    });
    algo.remove_peers_by_identity(&[identity]);
    if released_upload_slot {
        apply_choke_round(workers, peers, Some(algo));
    }
}

pub(crate) fn apply_choke_round(
    workers: &mut PeerGeneration,
    peers: &PeerSchedulingSnapshot,
    choking_algo: Option<&mut ChokingAlgorithm>,
) {
    let Some(algo) = choking_algo else { return };
    let actions = algo.rotate_choke_by_identity();
    let optimistic = algo.optimistically_unchoke_by_identity();
    for action in actions {
        let identity = action.identity();
        let choke = match action {
            IdentityChokeAction::Choke(_) => Some(true),
            IdentityChokeAction::Unchoke(_) => Some(false),
            IdentityChokeAction::NoChange(_) => None,
        };
        if let Some(choke) = choke
            && let Some(index) = peers.peer_index(identity)
            && let Some(actor_id) = peers.actor_id(index)
        {
            workers.apply_choke_action(actor_id, choke);
        }
    }
    if let Some(identity) = optimistic
        && let Some(index) = peers.peer_index(identity)
        && let Some(actor_id) = peers.actor_id(index)
    {
        workers.apply_choke_action(actor_id, false);
    }
}
