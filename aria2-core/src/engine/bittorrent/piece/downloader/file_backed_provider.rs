use crate::engine::bittorrent::peer::upload_session::PieceDataProvider;
use crate::engine::bittorrent::torrent::file_layout::MultiFileLayout;
use aria2_protocol::bittorrent::piece::bitfield::Bitfield;
use async_trait::async_trait;

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
    /// Optional live availability shared with an in-progress download.
    ///
    /// The backing file may contain pieces that are still being downloaded,
    /// so upload eligibility must follow the verified piece bitfield rather
    /// than assuming that the file is a complete seed.
    shared_pieces: Option<std::sync::Arc<std::sync::RwLock<Vec<u8>>>>,
    /// Write-back cache shared with the active single-file download writer.
    /// Verified data must be served from here before it reaches the file.
    write_cache: Option<std::sync::Arc<crate::filesystem::disk_cache::WrDiskCache>>,
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
            shared_pieces: None,
            write_cache: None,
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
            shared_pieces: None,
            write_cache: None,
        }
    }

    /// Create a provider whose availability follows a live BT download
    /// bitfield. The file is read only after the corresponding piece has been
    /// verified and published in `shared_pieces`.
    pub fn with_shared_bitfield(
        file_path: std::path::PathBuf,
        piece_length: u32,
        num_pieces: u32,
        multi_file_layout: Option<MultiFileLayout>,
        shared_pieces: std::sync::Arc<std::sync::RwLock<Vec<u8>>>,
    ) -> Self {
        Self {
            file_path,
            piece_length,
            num_pieces,
            multi_file_layout,
            pieces: Bitfield::new(num_pieces as usize),
            shared_pieces: Some(shared_pieces),
            write_cache: None,
        }
    }

    pub(crate) fn with_write_cache(
        mut self,
        write_cache: Option<std::sync::Arc<crate::filesystem::disk_cache::WrDiskCache>>,
    ) -> Self {
        self.write_cache = write_cache;
        self
    }

    /// Mark a piece as available (completed).
    pub fn mark_piece_available(&mut self, piece_index: u32) {
        let _ = self.pieces.set(piece_index as usize);
    }
}

impl FileBackedPieceProvider {
    async fn read_file_range(
        file_path: &std::path::Path,
        seek_pos: u64,
        len: u32,
    ) -> Option<Vec<u8>> {
        use std::io::SeekFrom;
        use tokio::fs::File;
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        let mut file = File::open(file_path).await.ok()?;
        file.seek(SeekFrom::Start(seek_pos)).await.ok()?;
        let mut buffer = vec![0u8; len as usize];
        file.read_exact(&mut buffer).await.ok()?;
        Some(buffer)
    }
}

#[async_trait]
impl PieceDataProvider for FileBackedPieceProvider {
    async fn get_piece_data(&self, piece_index: u32, offset: u32, length: u32) -> Option<Vec<u8>> {
        if !self.has_piece(piece_index) {
            return None;
        }

        if let Some(ref layout) = self.multi_file_layout {
            let global_start = piece_index as u64 * layout.piece_length() as u64 + offset as u64;

            if global_start >= layout.piece_space_size() {
                return None;
            }

            let actual_length =
                (length as u64).min(layout.piece_space_size() - global_start) as u32;
            let mut result = Vec::with_capacity(actual_length as usize);
            let mut current_global = global_start;
            let mut remaining = actual_length as u64;

            while remaining > 0 {
                let current_piece_idx = (current_global / layout.piece_length() as u64) as u32;
                let current_offset_in_piece =
                    (current_global % layout.piece_length() as u64) as u32;

                let Some((file_idx, file_offset)) =
                    layout.resolve_file_offset(current_piece_idx, current_offset_in_piece)
                else {
                    let next =
                        layout.next_content_offset(current_piece_idx, current_offset_in_piece);
                    let skip = (next - current_offset_in_piece).min(remaining as u32) as usize;
                    result.resize(result.len() + skip, 0);
                    current_global += skip as u64;
                    remaining -= skip as u64;
                    continue;
                };
                let file_path = layout.file_absolute_path(file_idx)?.to_path_buf();

                let file_info = layout.get_file_info(file_idx)?;
                let file_end = file_info.start_piece as u64 * layout.piece_length() as u64
                    + file_info.start_offset_in_piece as u64
                    + file_info.length;

                let bytes_available_in_file = file_end - current_global;
                let bytes_to_read = remaining.min(bytes_available_in_file) as u32;

                let data = Self::read_file_range(&file_path, file_offset, bytes_to_read).await?;
                result.extend_from_slice(&data);
                current_global += data.len() as u64;
                remaining -= data.len() as u64;
            }

            Some(result)
        } else {
            let file_pos = piece_index as u64 * self.piece_length as u64 + offset as u64;
            if let Some(cache) = &self.write_cache
                && let Ok(Some(data)) = cache.read(file_pos, u64::from(length)).await
            {
                return Some(data.to_vec());
            }
            Self::read_file_range(&self.file_path, file_pos, length).await
        }
    }

    fn has_piece(&self, piece_index: u32) -> bool {
        if let Some(shared_pieces) = &self.shared_pieces {
            return shared_pieces
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(piece_index as usize / 8)
                .is_some_and(|byte| byte & (1 << (7 - piece_index % 8)) != 0);
        }
        self.pieces.test(piece_index as usize)
    }

    fn num_pieces(&self) -> u32 {
        self.num_pieces
    }

    fn piece_length(&self) -> u32 {
        self.piece_length
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_provider_availability_tracks_verified_shared_bitfield() {
        let shared_pieces = std::sync::Arc::new(std::sync::RwLock::new(vec![0]));
        let provider = FileBackedPieceProvider::with_shared_bitfield(
            std::path::PathBuf::new(),
            16,
            3,
            None,
            std::sync::Arc::clone(&shared_pieces),
        );

        assert!(!provider.has_piece(0));
        assert!(!provider.has_piece(1));
        assert!(!provider.has_piece(2));

        *shared_pieces
            .write()
            .expect("write verified-piece bitfield") = vec![0b1010_0000];

        assert!(provider.has_piece(0));
        assert!(!provider.has_piece(1));
        assert!(provider.has_piece(2));
        assert!(!provider.has_piece(3));
    }
}
