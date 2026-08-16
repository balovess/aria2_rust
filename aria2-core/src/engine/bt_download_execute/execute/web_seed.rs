use tracing::{info, warn};

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_piece_downloader::write_piece_to_multi_files_coalesced;
use crate::error::Result;
use crate::util::rwlock_ext::RwLockRecover;

/// Attempt to download a piece from web seeds when peer download fails (BEP 19).
///
/// Returns `Ok(true)` if the piece was successfully downloaded and verified,
/// `Ok(false)` if the web seed download failed or hash verification failed.
#[allow(clippy::too_many_arguments)]
pub(super) async fn try_web_seed_fallback(
    cmd: &mut BtDownloadCommand,
    web_seed_manager: Option<&crate::engine::bt_web_seed::WebSeedManager>,
    next_piece_idx: usize,
    piece_manager: &mut aria2_protocol::bittorrent::piece::manager::PieceManager,
    piece_picker: &mut aria2_protocol::bittorrent::piece::picker::PiecePicker,
    writer: &mut Box<dyn crate::filesystem::disk_writer::SeekableDiskWriter>,
    piece_length: u32,
) -> Result<bool> {
    let ws_mgr = match web_seed_manager {
        Some(mgr) => mgr,
        None => return Ok(false),
    };

    info!(
        "[BT] Piece {} failed from peers, trying web seeds...",
        next_piece_idx
    );

    match ws_mgr.request_piece(next_piece_idx as u32).await {
        Ok(web_seed_data) => {
            info!(
                "[BT] Piece {} downloaded from web seed ({} bytes)",
                next_piece_idx,
                web_seed_data.len()
            );
            // Verify hash
            if piece_manager.verify_piece_hash(next_piece_idx as u32, &web_seed_data) {
                tracing::info!("[BT] Piece {} from web seed verified OK", next_piece_idx);
                piece_manager.mark_piece_complete(next_piece_idx as u32);
                piece_picker.mark_completed(next_piece_idx as u32);

                let web_seed_len = web_seed_data.len() as u64;
                let web_seed_bytes = bytes::Bytes::from(web_seed_data);
                if let Some(ref layout) = cmd.multi_file_layout {
                    write_piece_to_multi_files_coalesced(
                        layout,
                        next_piece_idx as u32,
                        &web_seed_bytes,
                        layout.piece_length(),
                    )
                    .await?;
                } else {
                    writer
                        .write_bytes_at(next_piece_idx as u64 * piece_length as u64, web_seed_bytes)
                        .await?;
                }

                let bitfield = piece_picker.export_bitfield();
                // Keep the in-memory status and on-disk checkpoint on the same
                // verified-piece snapshot regardless of the source adapter.
                {
                    let g = cmd.group.recover();
                    g.set_bt_bitfield(Some(bitfield.clone()));
                }

                cmd.completed_bytes += web_seed_len;
                cmd.persist_checkpoint_after_piece(writer, &bitfield)
                    .await?;
                Ok(true)
            } else {
                warn!(
                    "[BT] Piece {} from web seed failed hash verification",
                    next_piece_idx
                );
                Ok(false)
            }
        }
        Err(e) => {
            warn!(
                "[BT] Web seed download failed for piece {}: {}",
                next_piece_idx, e
            );
            Ok(false)
        }
    }
}
