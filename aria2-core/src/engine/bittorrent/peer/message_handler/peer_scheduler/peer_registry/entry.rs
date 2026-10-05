use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use tokio::sync::mpsc;

use crate::engine::bittorrent::peer::connection::{BtPeerConn, PeerActorId};
use crate::engine::bittorrent::peer::stats::PeerStats;
use crate::engine::bittorrent::peer::upload_session::PieceDataProvider;

use super::super::super::types::DEFAULT_MAX_OUTSTANDING_REQUEST;
use super::super::{PeerActorControl, PeerActorTask, PeerCommand, PeerEvent};

/// One long-lived I/O owner for a handshaken BitTorrent connection.
pub(crate) struct PeerActorEntry {
    pub(crate) actor_id: PeerActorId,
    /// Socket endpoint used for actor identity, deduplication, and cleanup.
    pub(crate) endpoint: SocketAddr,
    /// Remote listen endpoint suitable for RPC and peer exchange, if known.
    pub(crate) advertised_endpoint: Option<SocketAddr>,
    pub(crate) first_contact_time: std::time::Instant,
    pub(super) graceful_disconnected_at: Option<std::time::Instant>,
    pub(crate) dead: bool,
    pub(crate) metadata_pending: bool,
    pub(crate) incoming: bool,
    pub(crate) source: crate::request::request_group::BtPeerSource,
    pub(crate) client: Arc<std::sync::RwLock<Option<String>>>,
    pub(crate) pending_download_requests: Arc<AtomicUsize>,
    pub(crate) max_outstanding_requests: usize,
    pub(crate) stats: PeerStats,
    pub(crate) has_bitfield: bool,
    pub(crate) bitfield: Vec<u8>,
    pub(crate) peer_allowed_fast: HashSet<u32>,
    pub(crate) seeder: bool,
    pub(crate) ut_pex_id: Option<u8>,
    pub(super) actor: PeerActorTask,
}

impl PeerActorEntry {
    pub(crate) fn spawn(
        actor_id: PeerActorId,
        endpoint: SocketAddr,
        connection: BtPeerConn,
        dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        provider: Option<Arc<dyn PieceDataProvider>>,
        event_tx: mpsc::Sender<PeerEvent>,
    ) -> Self {
        let stats = connection.stats.clone();
        let seeder = connection.seeder;
        let incoming = connection.incoming;
        let metadata_pending = connection.is_metadata_pending();
        let advertised_endpoint = connection.advertised_endpoint();
        let first_contact_time = connection.first_contact_time();
        let source = connection.source;
        let client = connection.remote_client.clone();
        let pending_download_requests = Arc::new(AtomicUsize::new(0));
        let ut_pex_id = connection.peer_extension_id("ut_pex");
        let has_bitfield =
            !connection.is_metadata_pending() && connection.session_resource.is_some();
        let bitfield = connection
            .session_resource
            .as_ref()
            .map_or_else(Vec::new, |resource| resource.bitfield().to_vec());
        let peer_allowed_fast = connection.peer_allowed_fast_set().clone();
        let actor = PeerActorTask::spawn_owned(
            actor_id,
            connection,
            event_tx,
            dht_engine,
            provider,
            Arc::clone(&pending_download_requests),
            16,
        );

        Self {
            actor_id,
            endpoint,
            advertised_endpoint,
            first_contact_time,
            graceful_disconnected_at: None,
            dead: false,
            metadata_pending,
            incoming,
            source,
            client,
            pending_download_requests,
            max_outstanding_requests: DEFAULT_MAX_OUTSTANDING_REQUEST,
            stats,
            has_bitfield,
            bitfield,
            peer_allowed_fast,
            seeder,
            ut_pex_id,
            actor,
        }
    }

    pub(crate) fn handle(&self) -> PeerActorControl {
        self.actor.control.clone()
    }

    pub(crate) fn desired_upload_choked(&self) -> bool {
        self.actor.control.desired_upload_choked()
    }

    pub(crate) fn try_send(
        &self,
        command: PeerCommand,
    ) -> Result<(), mpsc::error::TrySendError<PeerCommand>> {
        self.actor.control.try_send(command)
    }

    pub(crate) async fn shutdown(&mut self) {
        let _ = self.actor.shutdown().await;
    }
}

pub(super) fn update_peer_availability(
    actor: &mut PeerActorEntry,
    piece_index: u32,
    has_piece: bool,
) {
    let byte_index = piece_index as usize / 8;
    if has_piece && actor.bitfield.len() <= byte_index {
        actor.bitfield.resize(byte_index + 1, 0);
    }
    if let Some(byte) = actor.bitfield.get_mut(byte_index) {
        let mask = 0x80 >> (piece_index % 8);
        if has_piece {
            *byte |= mask;
        } else {
            *byte &= !mask;
        }
    }
}
