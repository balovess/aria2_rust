use crate::engine::bt_upload_session::PieceDataProvider;
use crate::engine::multi_file_layout::MultiFileLayout;
use aria2_protocol::bittorrent::piece::bitfield::Bitfield;

/// Provides piece data from local files, used during seeding phase.
///
/// Supports both single-file and multi-file torrent layouts.
pub struct FileBackedPieceProvider {
    file_path: std::path::PathBuf,
    piece_length: u32,
    num_pieces: u32,
    multi_file_layout: Option<MultiFileLayout>,
    /// Per-piece availability, stored as one bit per piece.
    pieces: Bitfield,
}

impl FileBackedPieceProvider {
    /// Create a new `FileBackedPieceProvider` assuming all pieces are available
    /// (complete seed scenario).
    pub fn new(
        file_path: std::path::PathBuf,
        piece_length: u32,
        num_pieces: u32,
        multi_file_layout: Option<MultiFileLayout>,
    ) -> Self {
        let pieces = Bitfield::all_set(num_pieces as usize);
        Self {
            file_path,
            piece_length,
            num_pieces,
            multi_file_layout,
            pieces,
        }
    }

    /// Create a `FileBackedPieceProvider` with explicit piece availability.
    ///
    /// Use this for partial seeds where only some pieces are available.
    pub fn with_pieces(
        file_path: std::path::PathBuf,
        piece_length: u32,
        num_pieces: u32,
        multi_file_layout: Option<MultiFileLayout>,
        pieces: Vec<bool>,
    ) -> Self {
        debug_assert_eq!(pieces.len(), num_pieces as usize);
        let mut available = Bitfield::new(num_pieces as usize);
        for (index, is_available) in pieces.into_iter().enumerate() {
            if is_available {
                let _ = available.set(index);
            }
        }
        Self {
            file_path,
            piece_length,
            num_pieces,
            multi_file_layout,
            pieces: available,
        }
    }

    /// Mark a piece as available (completed).
    pub fn mark_piece_available(&mut self, piece_index: u32) {
        let _ = self.pieces.set(piece_index as usize);
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
            let global_start = piece_index as u64 * layout.piece_length() as u64 + offset as u64;

            if global_start >= layout.total_size() {
                return None;
            }

            let actual_length = (length as u64).min(layout.total_size() - global_start) as u32;
            let mut result = Vec::with_capacity(actual_length as usize);
            let mut current_global = global_start;
            let mut remaining = actual_length as u64;

            while remaining > 0 {
                let current_piece_idx = (current_global / layout.piece_length() as u64) as u32;
                let current_offset_in_piece =
                    (current_global % layout.piece_length() as u64) as u32;

                let (file_idx, file_offset) =
                    layout.resolve_file_offset(current_piece_idx, current_offset_in_piece)?;
                let file_path = layout.file_absolute_path(file_idx)?.to_path_buf();

                let file_info = layout.get_file_info(file_idx)?;
                let file_end = file_info.start_piece as u64 * layout.piece_length() as u64
                    + file_info.start_offset_in_piece as u64
                    + file_info.length;

                let bytes_available_in_file = file_end - current_global;
                let bytes_to_read = remaining.min(bytes_available_in_file) as u32;

                let data = read_op(file_path.clone(), file_offset, bytes_to_read)?;
                result.extend_from_slice(&data);
                current_global += data.len() as u64;
                remaining -= data.len() as u64;
            }

            Some(result)
        } else {
            let file_pos = piece_index as u64 * self.piece_length as u64 + offset as u64;
            read_op(self.file_path.clone(), file_pos, length)
        }
    }

    fn has_piece(&self, piece_index: u32) -> bool {
        self.pieces.test(piece_index as usize)
    }

    fn num_pieces(&self) -> u32 {
        self.num_pieces
    }

    fn piece_length(&self) -> u32 {
        self.piece_length
    }
}
