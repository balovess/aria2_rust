use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_piece::{PeerBitfieldTracker, PieceManager, PiecePicker};
use crate::engine::bt_piece_selector::BtPieceSelector;
use crate::engine::bt_web_seed::WebSeedManager;
use crate::error::Result;
use crate::filesystem::disk_writer::SeekableDiskWriter;

use crate::engine::bt_download_execute::types::{EndgameState, PeerKey};

use super::BtStopTimeoutState;

mod initialization;
mod piece;
mod run;

pub(super) struct PieceDownloadSession<'a> {
    pub(super) command: &'a mut BtDownloadCommand,
    pub(super) active_connections: &'a mut Vec<BtPeerConn>,
    pub(super) meta: &'a mut aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
    pub(super) piece_length: u32,
    pub(super) total_size: u64,
    pub(super) num_pieces: u32,
    pub(super) web_seed_manager: Option<&'a WebSeedManager>,
    pub(super) pex_enabled_peers: &'a mut HashSet<PeerKey>,
    pub(super) last_pex_send: &'a mut Instant,
    pub(super) pex_send_interval_secs: u64,
    pub(super) writer: Box<dyn SeekableDiskWriter>,
    pub(super) start_time: Instant,
    pub(super) last_speed_update: Instant,
    pub(super) last_completed: u64,
    pub(super) last_progress_save: Instant,
    pub(super) piece_selector: BtPieceSelector,
    pub(super) has_v1_piece_hashes: bool,
    pub(super) piece_manager: PieceManager,
    pub(super) piece_picker: PiecePicker,
    pub(super) completed_bitfield: Arc<RwLock<Vec<u8>>>,
    pub(super) peer_tracker: PeerBitfieldTracker,
    pub(super) endgame_state: EndgameState,
    pub(super) request_timeout: Duration,
    pub(super) peer_last_data_time: HashMap<PeerKey, Instant>,
    pub(super) last_snub_check: Instant,
    pub(super) stop_timeout: BtStopTimeoutState,
}

impl BtDownloadCommand {
    pub(in crate::engine::bt_download_execute::execute) async fn download_pieces_loop(
        &mut self,
        active_connections: &mut Vec<BtPeerConn>,
        meta: &mut aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        piece_length: u32,
        total_size: u64,
        num_pieces: u32,
        web_seed_manager: Option<&WebSeedManager>,
        pex_enabled_peers: &mut HashSet<PeerKey>,
        last_pex_send: &mut Instant,
        pex_send_interval_secs: u64,
        verified_piece_indices: &[usize],
    ) -> Result<()> {
        if verified_piece_indices.len() == num_pieces as usize {
            tracing::info!("[BT] All torrent pieces are already complete; skipping piece writer");
            return Ok(());
        }
        let session = PieceDownloadSession::new(
            self,
            active_connections,
            meta,
            piece_length,
            total_size,
            num_pieces,
            web_seed_manager,
            pex_enabled_peers,
            last_pex_send,
            pex_send_interval_secs,
            verified_piece_indices,
        )?;
        session.run().await
    }
}
