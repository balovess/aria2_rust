use crate::engine::bt_upload_session::PieceDataProvider;
use crate::engine::multi_file_layout::MultiFileLayout;
use crate::error::{Aria2Error, FatalError, Result};
use std::collections::HashMap;

/// Manages piece/block download operations for BitTorrent downloads.
///
/// This module encapsulates:
/// - Piece selection and block request logic
/// - Data verification and hash checking
/// - Writing downloaded data to disk or multi-file layouts
/// - File-backed piece provider for seeding phase
///
/// Extracted from BtDownloadCommand to follow single responsibility principle,
/// mirroring original aria2 C++ architecture separation.

// ======================================================================
// File-Backed Piece Provider
// ======================================================================

/// Provides piece data from local files, used during seeding phase.
///
/// Supports both single-file and multi-file torrent layouts.
pub struct FileBackedPieceProvider {
    file_path: std::path::PathBuf,
    piece_length: u32,
    num_pieces: u32,
    multi_file_layout: Option<MultiFileLayout>,
}

impl FileBackedPieceProvider {
    pub fn new(
        file_path: std::path::PathBuf,
        piece_length: u32,
        num_pieces: u32,
        multi_file_layout: Option<MultiFileLayout>,
    ) -> Self {
        Self {
            file_path,
            piece_length,
            num_pieces,
            multi_file_layout,
        }
    }
}

impl PieceDataProvider for FileBackedPieceProvider {
    fn get_piece_data(&self, piece_index: u32, offset: u32, length: u32) -> Option<Vec<u8>> {
        use std::io::SeekFrom;
        use tokio::fs::File;
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        let read_op =
            move |file_path: std::path::PathBuf, seek_pos: u64, len: u32| -> Option<Vec<u8>> {
                let rt = match tokio::runtime::Handle::try_current() {
                    Ok(handle) => handle,
                    Err(_) => {
                        let rt = tokio::runtime::Runtime::new().ok()?;
                        return rt.block_on(async {
                            let mut f = File::open(&file_path).await.ok()?;
                            f.seek(SeekFrom::Start(seek_pos)).await.ok()?;
                            let mut buf = vec![0u8; len as usize];
                            f.read_exact(&mut buf).await.ok()?;
                            Some(buf)
                        });
                    }
                };
                tokio::task::block_in_place(|| {
                    rt.block_on(async {
                        let mut f = File::open(&file_path).await.ok()?;
                        f.seek(SeekFrom::Start(seek_pos)).await.ok()?;
                        let mut buf = vec![0u8; len as usize];
                        f.read_exact(&mut buf).await.ok()?;
                        Some(buf)
                    })
                })
            };

        if let Some(ref layout) = self.multi_file_layout {
            let (file_idx, file_offset) = layout.resolve_file_offset(piece_index, offset)?;
            let file_path = layout.file_absolute_path(file_idx)?.to_path_buf();
            read_op(file_path, file_offset, length)
        } else {
            let file_pos = piece_index as u64 * self.piece_length as u64 + offset as u64;
            read_op(self.file_path.clone(), file_pos, length)
        }
    }

    fn has_piece(&self, _piece_index: u32) -> bool {
        true
    }

    fn num_pieces(&self) -> u32 {
        self.num_pieces
    }
}

// ======================================================================
// Multi-File Writer
// ======================================================================

/// Writes a completed piece's data across multiple files in a multi-file torrent.
///
/// Handles cross-file boundary cases where a single piece spans multiple files.
/// Each file is opened once, written to at the correct offsets, then flushed.
///
/// # Arguments
/// * `layout` - The multi-file layout defining file boundaries
/// * `piece_idx` - Index of the piece being written
/// * `piece_data` - Complete piece data to write
/// * `_piece_length` - Standard piece length (reserved for future use)
///
/// # Errors
/// Returns error if file open, seek, write, or flush operations fail.
pub async fn write_piece_to_multi_files(
    layout: &MultiFileLayout,
    piece_idx: u32,
    piece_data: &[u8],
    _piece_length: u32,
) -> Result<()> {
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    let mut file_writers: HashMap<usize, tokio::fs::File> = HashMap::new();

    let mut data_offset = 0usize;
    while data_offset < piece_data.len() {
        let piece_offset = data_offset as u32;

        if let Some((file_idx, file_offset)) = layout.resolve_file_offset(piece_idx, piece_offset) {
            let file_path = layout
                .file_absolute_path(file_idx)
                .ok_or_else(|| Aria2Error::Fatal(FatalError::Config("invalid file index".to_string())))?
                .to_path_buf();

            if !file_writers.contains_key(&file_idx) {
                let f = tokio::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .open(&file_path)
                    .await
                    .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("open failed: {}", e))))?;
                file_writers.insert(file_idx, f);
            }

            let file_info = layout
                .get_file_info(file_idx)
                .ok_or_else(|| Aria2Error::Fatal(FatalError::Config("invalid file index".to_string())))?;

            let bytes_available_in_file = file_info.length.saturating_sub(file_offset);
            let bytes_remaining_in_piece = (piece_data.len() - data_offset) as u64;
            let write_len = bytes_available_in_file.min(bytes_remaining_in_piece) as usize;

            if write_len == 0 {
                break;
            }

            let chunk = &piece_data[data_offset..data_offset + write_len];

            let writer = file_writers.get_mut(&file_idx).unwrap();
            writer
                .seek(std::io::SeekFrom::Start(file_offset))
                .await
                .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("seek failed: {}", e))))?;
            writer
                .write_all(chunk)
                .await
                .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("write failed: {}", e))))?;

            data_offset += write_len;
        } else {
            break;
        }
    }

    for (_, mut f) in file_writers {
        f.flush()
            .await
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("flush failed: {}", e))))?;
    }

    Ok(())
}
