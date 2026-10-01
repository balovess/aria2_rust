use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::engine::bittorrent::download::command::BtDownloadCommand;
use crate::engine::bittorrent::download::web_seed::WebSeedManager;
use crate::engine::bittorrent::peer::message_handler::PeerSwarm;
use crate::engine::bittorrent::piece::selector::BtPieceSelector;
use crate::engine::bittorrent::piece::{PeerBitfieldTracker, PieceManager, PiecePicker};
use crate::error::Result;
use crate::filesystem::disk_writer::SeekableDiskWriter;

use crate::engine::bittorrent::download::execute::incoming::PeerActorAdmissionContext;
use crate::engine::bittorrent::download::execute::types::{EndgameState, PeerKey};

use super::BtStopTimeoutState;
use crate::engine::bittorrent::download::execute::peer_session::TorrentSession;

mod availability;
mod download_speed;
mod initialization;
pub(in crate::engine::bittorrent::download::execute) mod peer_dials;
mod piece;
mod run;

pub(super) struct PieceDownloadSession<'a> {
    pub(super) command: &'a mut BtDownloadCommand,
    pub(super) swarm: &'a mut PeerSwarm,
    pub(super) meta: &'a aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
    pub(super) network_info_hash: [u8; 20],
    pub(super) piece_length: u32,
    pub(super) total_size: u64,
    pub(super) num_pieces: u32,
    pub(super) web_seed_manager: Option<Arc<WebSeedManager>>,
    pub(super) pending_pex_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    pub(super) pending_tracker_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    pub(super) last_pex_send: &'a mut Instant,
    pub(super) writer: Box<dyn SeekableDiskWriter>,
    pub(super) start_time: Instant,
    pub(super) last_uploaded: u64,
    pub(super) upload_counter: Arc<std::sync::atomic::AtomicU64>,
    pub(super) last_progress_save: Instant,
    pub(super) piece_selector: BtPieceSelector,
    pub(super) has_v1_piece_hashes: bool,
    pub(super) piece_manager: PieceManager,
    pub(super) piece_picker: PiecePicker,
    pub(super) completed_bitfield: Arc<RwLock<Vec<u8>>>,
    pub(super) upload_provider:
        Arc<dyn crate::engine::bittorrent::peer::upload_session::PieceDataProvider>,
    pub(super) peer_tracker: PeerBitfieldTracker,
    pub(super) endgame_state: EndgameState,
    pub(super) request_timeout: Duration,
    pub(super) peer_last_data_time: HashMap<PeerKey, Instant>,
    pub(super) last_snub_check: Instant,
    pub(super) stop_timeout: BtStopTimeoutState,
    pub(super) in_flight_pieces:
        HashMap<u32, crate::filesystem::control_file::ControlFileInFlightPiece>,
}

/// Tells the outer loop whether this piece iteration reached its normal
/// progress-update boundary or should be retried immediately.
pub(super) enum PieceLoopAction {
    RefreshProgress,
    Retry,
}

impl PieceDownloadSession<'_> {
    pub(super) fn peer_actor_admission_context(&self) -> PeerActorAdmissionContext {
        PeerActorAdmissionContext {
            network_info_hash: self.network_info_hash,
            piece_length: self.piece_length,
            num_pieces: self.num_pieces,
            total_size: self.total_size,
            provider: Arc::clone(&self.upload_provider),
            upload_counter: Arc::clone(&self.upload_counter),
        }
    }
}

impl BtDownloadCommand {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::engine::bittorrent::download::execute) async fn download_pieces_loop(
        &mut self,
        torrent_session: &mut TorrentSession,
        meta: &mut aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        piece_length: u32,
        total_size: u64,
        num_pieces: u32,
        verified_piece_indices: &[usize],
    ) -> Result<()> {
        if verified_piece_indices.len() == num_pieces as usize {
            tracing::info!("[BT] All torrent pieces are already complete; skipping piece writer");
            return Ok(());
        }
        let session = PieceDownloadSession::new(
            self,
            std::mem::take(&mut torrent_session.initial_peers),
            torrent_session.network_info_hash,
            &mut torrent_session.swarm,
            Arc::clone(&torrent_session.upload_counter),
            meta,
            piece_length,
            total_size,
            num_pieces,
            torrent_session.web_seed_manager.clone(),
            &mut torrent_session.last_pex_send,
            verified_piece_indices,
        )
        .await?;
        let result = session.run().await;
        if result.is_err() {
            torrent_session.swarm.shutdown_all().await;
        }
        result
    }
}
