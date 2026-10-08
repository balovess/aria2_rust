use crate::error::{Aria2Error, Result};
use std::path::{Path, PathBuf};

mod native;
mod persistence;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlFileInFlightPiece {
    pub index: u32,
    pub length: u32,
    pub bitfield: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ControlFile {
    path: PathBuf,
    total_length: u64,
    completed_length: u64,
    upload_length: u64,
    bitfield: Vec<u8>,
    in_flight_pieces: Vec<ControlFileInFlightPiece>,
    num_pieces: usize,
    checksum_algo: u8,
    checksum_value: Vec<u8>,
    torrent_checkpoint: bool,
    torrent_info_hash: Option<[u8; 20]>,
    torrent_piece_length: Option<u32>,
    native_piece_length: Option<u32>,
    native_layout: bool,
}

impl ControlFile {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    /// Piece length explicitly represented by this native checkpoint layout.
    ///
    /// Legacy A2CF checkpoints did not store a generic piece length, so they
    /// return `None` and must not be interpreted using a new segment layout.
    pub fn piece_length(&self) -> Option<u32> {
        self.native_piece_length.or(self.torrent_piece_length)
    }
    pub fn completed_length(&self) -> u64 {
        self.completed_length
    }
    pub fn bitfield(&self) -> &[u8] {
        &self.bitfield
    }

    pub fn in_flight_pieces(&self) -> &[ControlFileInFlightPiece] {
        &self.in_flight_pieces
    }

    pub fn set_in_flight_pieces(&mut self, pieces: Vec<ControlFileInFlightPiece>) {
        self.in_flight_pieces = pieces;
    }
    pub fn set_checksum(&mut self, algo: u8, value: Vec<u8>) {
        self.checksum_algo = algo;
        self.checksum_value = value;
    }
    pub fn checksum_algo(&self) -> u8 {
        self.checksum_algo
    }

    pub fn checksum_value(&self) -> &[u8] {
        &self.checksum_value
    }

    pub fn upload_length(&self) -> u64 {
        self.upload_length
    }

    /// Whether this value was decoded from aria2's native v0/v1 layout.
    pub fn uses_native_layout(&self) -> bool {
        self.native_layout
    }

    /// Mark this Rust-owned checkpoint as BitTorrent state.
    ///
    /// The marker prevents a generic HTTP/FTP checkpoint with the same output
    /// path and shape from being mistaken for verified torrent pieces.
    pub fn mark_torrent_checkpoint(&mut self) {
        self.torrent_checkpoint = true;
    }

    pub fn is_torrent_checkpoint(&self) -> bool {
        self.torrent_checkpoint
    }

    /// Store the torrent identity for a Rust-owned BitTorrent checkpoint.
    pub fn set_torrent_info_hash(&mut self, info_hash: [u8; 20]) {
        self.torrent_info_hash = Some(info_hash);
    }

    pub fn torrent_info_hash(&self) -> Option<[u8; 20]> {
        self.torrent_info_hash
    }

    /// Store the piece length used by a Rust-owned BitTorrent checkpoint.
    pub fn set_torrent_piece_length(&mut self, piece_length: u32) {
        self.torrent_piece_length = Some(piece_length);
    }

    pub fn torrent_piece_length(&self) -> Option<u32> {
        self.torrent_piece_length
    }

    /// Replace the persisted piece bitfield with a complete snapshot.
    pub fn set_bitfield(&mut self, bitfield: Vec<u8>) {
        self.bitfield = bitfield;
    }

    pub(crate) fn normalize_legacy_piece_layout(&mut self, num_pieces: usize) -> Result<()> {
        if !self.native_layout {
            self.normalize_bitfield(num_pieces)?;
        }
        Ok(())
    }

    pub async fn open_or_create(
        ctrl_path: &Path,
        total_length: u64,
        num_pieces: usize,
    ) -> Result<Self> {
        if ctrl_path.exists() {
            let mut control_file = Self::load(ctrl_path).await?.ok_or_else(|| {
                Aria2Error::FileIo(format!(
                    "Failed to load control file: {}",
                    ctrl_path.display()
                ))
            })?;
            // Legacy A2CF has no official piece-length field, so restore its
            // logical layout at the typed open seam. Native files retain the
            // piece layout encoded by aria2 itself.
            if !control_file.native_layout {
                control_file.normalize_bitfield(num_pieces)?;
            }
            Ok(control_file)
        } else {
            let piece_length = piece_length_for(total_length, num_pieces)?;
            Self::new_with_piece_length(ctrl_path, total_length, piece_length)
        }
    }

    /// Open an existing checkpoint only when it uses this exact fixed-piece
    /// layout, or create a new checkpoint with the supplied piece length.
    pub async fn open_or_create_with_piece_length(
        ctrl_path: &Path,
        total_length: u64,
        piece_length: u32,
    ) -> Result<Self> {
        if piece_length == 0 {
            return Err(Aria2Error::InvalidArgument(
                "Control file piece length must not be 0".to_string(),
            ));
        }
        if ctrl_path.exists() {
            let control_file = Self::load(ctrl_path).await?.ok_or_else(|| {
                Aria2Error::FileIo(format!(
                    "Failed to load control file: {}",
                    ctrl_path.display()
                ))
            })?;
            let expected_pieces = piece_count(total_length, piece_length)?;
            if control_file.total_length() == total_length
                && control_file.piece_length() == Some(piece_length)
                && control_file.bitfield().len() == expected_pieces.div_ceil(8)
            {
                return Ok(control_file);
            }
            return Err(Aria2Error::InvalidArgument(format!(
                "Control file layout does not match piece length {}: {}",
                piece_length,
                ctrl_path.display()
            )));
        }
        Self::new_with_piece_length(ctrl_path, total_length, piece_length)
    }

    fn new_with_piece_length(
        ctrl_path: &Path,
        total_length: u64,
        piece_length: u32,
    ) -> Result<Self> {
        let actual_num_pieces = piece_count(total_length, piece_length)?;
        let bitfield_len = actual_num_pieces.div_ceil(8);
        Ok(Self {
            path: ctrl_path.to_path_buf(),
            total_length,
            completed_length: 0,
            upload_length: 0,
            bitfield: vec![0u8; bitfield_len],
            in_flight_pieces: Vec::new(),
            num_pieces: actual_num_pieces,
            checksum_algo: 0,
            checksum_value: Vec::new(),
            torrent_checkpoint: false,
            torrent_info_hash: None,
            torrent_piece_length: None,
            native_piece_length: Some(piece_length),
            native_layout: false,
        })
    }

    pub fn mark_piece_done(&mut self, index: usize) {
        let byte_index = index / 8;
        let bit_index = index % 8;
        if index < self.num_pieces && byte_index < self.bitfield.len() {
            self.bitfield[byte_index] |= 1 << (7 - bit_index);
            self.in_flight_pieces
                .retain(|piece| piece.index as usize != index);
            self.completed_length = self.calculate_completed();
        }
    }

    fn normalize_bitfield(&mut self, num_pieces: usize) -> Result<()> {
        let piece_length = piece_length_for(self.total_length, num_pieces)?;
        self.native_piece_length = Some(piece_length);
        self.num_pieces = piece_count(self.total_length, piece_length)?;
        self.bitfield.resize(self.num_pieces.div_ceil(8), 0);

        if let Some(last_byte) = self.bitfield.last_mut() {
            let valid_bits = self.num_pieces % 8;
            if valid_bits != 0 {
                *last_byte &= u8::MAX << (8 - valid_bits);
            }
        }

        self.completed_length = self.calculate_completed();
        Ok(())
    }

    pub fn is_piece_done(&self, index: usize) -> bool {
        let byte_index = index / 8;
        let bit_index = index % 8;
        if byte_index < self.bitfield.len() {
            (self.bitfield[byte_index] & (1 << (7 - bit_index))) != 0
        } else {
            false
        }
    }

    pub fn completed_pieces(&self) -> usize {
        self.bitfield.iter().map(|b| b.count_ones() as usize).sum()
    }

    fn calculate_completed(&self) -> u64 {
        if self.total_length == 0 || self.num_pieces == 0 {
            return 0;
        }
        let piece_size = self
            .torrent_piece_length
            .or(self.native_piece_length)
            .map(u64::from)
            .unwrap_or_else(|| self.total_length.div_ceil(self.num_pieces as u64));
        (0..self.num_pieces)
            .filter(|&index| self.is_piece_done(index))
            .map(|index| {
                let offset = index as u64 * piece_size;
                self.total_length.saturating_sub(offset).min(piece_size)
            })
            .sum()
    }

    pub fn update_completed_length(&mut self, length: u64) {
        self.completed_length = length.min(self.total_length);
    }

    fn effective_piece_length(&self) -> Result<u32> {
        self.torrent_piece_length
            .or(self.native_piece_length)
            .or_else(|| piece_length_for(self.total_length, self.num_pieces).ok())
            .filter(|piece_length| *piece_length != 0)
            .ok_or_else(|| {
                Aria2Error::InvalidArgument(
                    "Control file piece length cannot be represented".to_string(),
                )
            })
    }

    pub fn control_path_for(output_path: &Path) -> PathBuf {
        // aria2 appends the suffix.  Replacing the extension would turn
        // `file.tar.gz` into `file.tar.aria2`, while the documented sidecar
        // is `file.tar.gz.aria2`.  The same rule applies to a multi-file
        // torrent's top directory (`top-directory.aria2`).
        if output_path == Path::new(".") || output_path == Path::new("..") {
            return output_path.join(".aria2");
        }

        let Some(file_name) = output_path.file_name() else {
            return output_path.join(".aria2");
        };
        let mut control_name = file_name.to_os_string();
        control_name.push(".aria2");
        output_path.with_file_name(control_name)
    }
}

pub(super) fn checksum_length(algo: u8) -> Option<usize> {
    match algo {
        1 => Some(16), // MD5
        2 => Some(20), // SHA-1
        3 => Some(32), // SHA-256
        4 => Some(8),  // CRC-64
        _ => None,
    }
}

pub(super) fn piece_length_for(total_length: u64, num_pieces: usize) -> Result<u32> {
    let divisor = num_pieces.max(1) as u64;
    let piece_length = if total_length == 0 {
        1
    } else {
        total_length.div_ceil(divisor)
    };
    u32::try_from(piece_length).map_err(|_| {
        Aria2Error::InvalidArgument(format!(
            "Control file piece length exceeds native aria2 limit: {}",
            piece_length
        ))
    })
}

pub(super) fn piece_count(total_length: u64, piece_length: u32) -> Result<usize> {
    if piece_length == 0 {
        return Err(Aria2Error::InvalidArgument(
            "Control file piece length must not be 0".to_string(),
        ));
    }
    usize::try_from(total_length.div_ceil(piece_length as u64))
        .map_err(|_| Aria2Error::InvalidArgument("Control file has too many pieces".to_string()))
}

pub(super) fn validate_in_flight_pieces(
    pieces: &[ControlFileInFlightPiece],
    total_length: u64,
    piece_length: u32,
    num_pieces: usize,
    _completed_bitfield: &[u8],
) -> Result<()> {
    let mut seen = std::collections::HashSet::with_capacity(pieces.len());
    for piece in pieces {
        let index = piece.index as usize;
        if index >= num_pieces {
            return Err(Aria2Error::FileIo(format!(
                "In-flight torrent piece index is out of range: {}",
                piece.index
            )));
        }
        if !seen.insert(piece.index) {
            return Err(Aria2Error::FileIo(format!(
                "Duplicate in-flight torrent piece index: {}",
                piece.index
            )));
        }
        if piece.length == 0 || piece.length > piece_length {
            return Err(Aria2Error::FileIo(format!(
                "In-flight torrent piece length is out of range: {}",
                piece.length
            )));
        }
        let block_count = (piece.length as usize).div_ceil(16 * 1024);
        let expected_bitfield_length = block_count.div_ceil(8);
        if piece.bitfield.len() != expected_bitfield_length {
            return Err(Aria2Error::FileIo(format!(
                "In-flight block bitfield length mismatch: expected {}, actual {}",
                expected_bitfield_length,
                piece.bitfield.len()
            )));
        }
        let unused_bits = (8 - block_count % 8) % 8;
        if unused_bits != 0
            && piece
                .bitfield
                .last()
                .is_some_and(|byte| byte & ((1u8 << unused_bits) - 1) != 0)
        {
            return Err(Aria2Error::FileIo(
                "In-flight block bitfield has set trailing bits".to_string(),
            ));
        }
        let piece_offset = u64::from(piece.index) * u64::from(piece_length);
        if piece_offset >= total_length {
            return Err(Aria2Error::FileIo(format!(
                "In-flight torrent piece offset is out of range: {}",
                piece.index
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
