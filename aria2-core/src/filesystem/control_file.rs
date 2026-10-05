use crate::error::{Aria2Error, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

// A2CF was the pre-release Rust-only write format. Keep its reader for
// migration compatibility, but do not add new fields to it: it will be
// removed after old checkpoints have aged out.
const CONTROL_MAGIC: &[u8; 4] = b"A2CF";
const CONTROL_VERSION: u16 = 1;
const FLAG_HAS_CHECKSUM: u8 = 0x01;
const FLAG_TORRENT_CHECKPOINT: u8 = 0x02;
const FLAG_TORRENT_INFO_HASH: u8 = 0x04;
const FLAG_TORRENT_PIECE_LENGTH: u8 = 0x08;
const CONTROL_HEADER_LEN: usize = 39;

// The prefix of every newly-written file is the official aria2 v1 format.
// A2RX is only the project's extension trailer; aria2's reader stops after
// the official in-flight records and therefore remains compatible with it.
const PROJECT_EXTENSION_MAGIC: &[u8; 4] = b"A2RX";
const PROJECT_EXTENSION_VERSION: u16 = 1;
const PROJECT_EXTENSION_HEADER_LEN: usize = 12;
const PROJECT_EXTENSION_DIGEST_LEN: usize = 32;
const EXT_HAS_COMPLETED_LENGTH: u16 = 0x0001;
const EXT_HAS_CHECKSUM: u16 = 0x0002;
const EXT_TORRENT_CHECKPOINT: u16 = 0x0004;
const EXT_HAS_INFO_HASH: u16 = 0x0008;
const EXT_HAS_PIECE_LENGTH: u16 = 0x0010;
const EXT_KNOWN_FLAGS: u16 = EXT_HAS_COMPLETED_LENGTH
    | EXT_HAS_CHECKSUM
    | EXT_TORRENT_CHECKPOINT
    | EXT_HAS_INFO_HASH
    | EXT_HAS_PIECE_LENGTH;

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

    pub async fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }

        let data = tokio::fs::read(path)
            .await
            .map_err(|e| Aria2Error::FileIo(format!("{}: {e}", path.display())))?;

        if data.len() < 2 {
            return Err(Aria2Error::FileIo(format!(
                "Truncated control file header: {} bytes (expected at least 2)",
                data.len(),
            )));
        }

        // Native aria2 control files start with version 0000 or 0001. Read
        // both versions so a download can be resumed after switching
        // clients; all future writes use the native v1 prefix below.
        if data[0] == 0 && (data[1] == 0 || data[1] == 1) {
            return parse_native_control_file(path, &data);
        }

        if data.len() < CONTROL_HEADER_LEN {
            return Err(Aria2Error::FileIo(format!(
                "Truncated control file header: {} bytes (expected at least {})",
                data.len(),
                CONTROL_HEADER_LEN
            )));
        }

        if &data[0..4] != CONTROL_MAGIC {
            return Err(Aria2Error::FileIo("Invalid control file magic".to_string()));
        }

        let version = u16_from_le(&data[4..6]);
        if version > CONTROL_VERSION {
            return Err(Aria2Error::FileIo(format!(
                "Unsupported version: {}",
                version
            )));
        }

        // TODO: Remove the legacy A2CF reader after the migration window.
        let flags = data[6];
        let known_flags = FLAG_HAS_CHECKSUM
            | FLAG_TORRENT_CHECKPOINT
            | FLAG_TORRENT_INFO_HASH
            | FLAG_TORRENT_PIECE_LENGTH;
        if flags & !known_flags != 0 {
            return Err(Aria2Error::FileIo(format!(
                "Unknown control file flags: 0x{:02x}",
                flags & !known_flags
            )));
        }
        let total_length = u64_from_le(&data[7..15]);
        let completed_length = u64_from_le(&data[15..23]);
        let upload_length = u64_from_le(&data[23..31]);
        let bitfield_length = usize::try_from(u64_from_le(&data[31..39])).map_err(|_| {
            Aria2Error::FileIo("Control file bitfield length exceeds platform limits".to_string())
        })?;

        let mut offset = CONTROL_HEADER_LEN;
        let checksum_algo = if flags & FLAG_HAS_CHECKSUM != 0 {
            let algo = *data.get(offset).ok_or_else(|| {
                Aria2Error::FileIo("Truncated control file checksum algorithm".to_string())
            })?;
            offset += 1;
            algo
        } else {
            0
        };

        let checksum_value = if flags & FLAG_HAS_CHECKSUM != 0 {
            let len = checksum_length(checksum_algo).ok_or_else(|| {
                Aria2Error::FileIo(format!(
                    "Unsupported control file checksum algorithm: {}",
                    checksum_algo
                ))
            })?;
            let end = offset.checked_add(len).ok_or_else(|| {
                Aria2Error::FileIo("Control file checksum length overflow".to_string())
            })?;
            if end > data.len() {
                return Err(Aria2Error::FileIo(
                    "Truncated control file checksum".to_string(),
                ));
            }
            let val = data[offset..end].to_vec();
            offset = end;
            val
        } else {
            Vec::new()
        };

        let torrent_info_hash = if flags & FLAG_TORRENT_INFO_HASH != 0 {
            let end = offset.checked_add(20).ok_or_else(|| {
                Aria2Error::FileIo("Control file torrent info hash length overflow".to_string())
            })?;
            if end > data.len() {
                return Err(Aria2Error::FileIo(
                    "Truncated control file torrent info hash".to_string(),
                ));
            }
            let mut info_hash = [0u8; 20];
            info_hash.copy_from_slice(&data[offset..end]);
            offset = end;
            Some(info_hash)
        } else {
            None
        };

        let torrent_piece_length = if flags & FLAG_TORRENT_PIECE_LENGTH != 0 {
            let end = offset.checked_add(4).ok_or_else(|| {
                Aria2Error::FileIo("Control file torrent piece length overflow".to_string())
            })?;
            if end > data.len() {
                return Err(Aria2Error::FileIo(
                    "Truncated control file torrent piece length".to_string(),
                ));
            }
            let piece_length = u32_from_le(&data[offset..end]);
            if piece_length == 0 {
                return Err(Aria2Error::FileIo(
                    "Control file torrent piece length must not be 0".to_string(),
                ));
            }
            offset = end;
            Some(piece_length)
        } else {
            None
        };

        let bitfield_end = offset.checked_add(bitfield_length).ok_or_else(|| {
            Aria2Error::FileIo("Control file bitfield length overflow".to_string())
        })?;
        if bitfield_end != data.len() {
            return Err(Aria2Error::FileIo(format!(
                "Control file bitfield length mismatch: declared {}, available {}",
                bitfield_length,
                data.len().saturating_sub(offset)
            )));
        }
        let bitfield = data[offset..bitfield_end].to_vec();
        let num_pieces = bitfield.len() * 8;
        if completed_length > total_length {
            return Err(Aria2Error::FileIo(
                "Control file completed length exceeds total length".to_string(),
            ));
        }

        Ok(Some(Self {
            path: path.to_path_buf(),
            total_length,
            completed_length,
            upload_length,
            bitfield,
            in_flight_pieces: Vec::new(),
            num_pieces,
            checksum_algo,
            checksum_value,
            torrent_checkpoint: flags & FLAG_TORRENT_CHECKPOINT != 0,
            torrent_info_hash,
            torrent_piece_length,
            native_piece_length: None,
            native_layout: false,
        }))
    }

    pub async fn save(&self) -> Result<()> {
        let piece_length = self.effective_piece_length()?;
        let num_pieces = piece_count(self.total_length, piece_length)?;
        if self.num_pieces != num_pieces {
            return Err(Aria2Error::InvalidArgument(format!(
                "Control file piece count mismatch: expected {}, actual {}",
                num_pieces, self.num_pieces
            )));
        }
        let bitfield_length = num_pieces.div_ceil(8);
        if self.bitfield.len() != bitfield_length {
            return Err(Aria2Error::InvalidArgument(format!(
                "Control file bitfield length mismatch: expected {}, actual {}",
                bitfield_length,
                self.bitfield.len()
            )));
        }
        if let Some(last_byte) = self.bitfield.last()
            && !num_pieces.is_multiple_of(8)
            && last_byte & (u8::MAX >> (num_pieces % 8)) != 0
        {
            return Err(Aria2Error::InvalidArgument(
                "Control file bitfield has set bits outside the piece count".to_string(),
            ));
        }
        validate_in_flight_pieces(
            &self.in_flight_pieces,
            self.total_length,
            piece_length,
            num_pieces,
            &self.bitfield,
        )?;

        let is_torrent = self.torrent_checkpoint
            || self.torrent_info_hash.is_some()
            || self.torrent_piece_length.is_some();
        if is_torrent && self.torrent_info_hash.is_none() {
            return Err(Aria2Error::InvalidArgument(
                "Torrent control files require an info hash".to_string(),
            ));
        }
        let bitfield_length = u32::try_from(bitfield_length).map_err(|_| {
            Aria2Error::InvalidArgument("Control file bitfield is too large".to_string())
        })?;
        let extension = serialize_project_extension(self)?;
        let tmp_path = self.path.with_extension("aria2.tmp");
        let mut file = tokio::fs::File::create(&tmp_path)
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        file.write_all(&1u16.to_be_bytes())
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        file.write_all(&(if is_torrent { 1u32 } else { 0 }).to_be_bytes())
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        if let Some(info_hash) = self.torrent_info_hash {
            file.write_all(&20u32.to_be_bytes())
                .await
                .map_err(|e| Aria2Error::Io(e.to_string()))?;
            file.write_all(&info_hash)
                .await
                .map_err(|e| Aria2Error::Io(e.to_string()))?;
        } else {
            file.write_all(&0u32.to_be_bytes())
                .await
                .map_err(|e| Aria2Error::Io(e.to_string()))?;
        }
        file.write_all(&piece_length.to_be_bytes())
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        file.write_all(&self.total_length.to_be_bytes())
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        file.write_all(&self.upload_length.to_be_bytes())
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        file.write_all(&bitfield_length.to_be_bytes())
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        file.write_all(&self.bitfield)
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        let in_flight_count = u32::try_from(self.in_flight_pieces.len()).map_err(|_| {
            Aria2Error::InvalidArgument("Too many in-flight torrent pieces".to_string())
        })?;
        file.write_all(&in_flight_count.to_be_bytes())
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        for piece in &self.in_flight_pieces {
            file.write_all(&piece.index.to_be_bytes())
                .await
                .map_err(|e| Aria2Error::Io(e.to_string()))?;
            file.write_all(&piece.length.to_be_bytes())
                .await
                .map_err(|e| Aria2Error::Io(e.to_string()))?;
            let bitfield_length = u32::try_from(piece.bitfield.len()).map_err(|_| {
                Aria2Error::InvalidArgument("In-flight block bitfield is too large".to_string())
            })?;
            file.write_all(&bitfield_length.to_be_bytes())
                .await
                .map_err(|e| Aria2Error::Io(e.to_string()))?;
            file.write_all(&piece.bitfield)
                .await
                .map_err(|e| Aria2Error::Io(e.to_string()))?;
        }
        file.write_all(&extension)
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        file.sync_all()
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        drop(file);
        tokio::fs::rename(&tmp_path, &self.path)
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        Ok(())
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

fn checksum_length(algo: u8) -> Option<usize> {
    match algo {
        1 => Some(16), // MD5
        2 => Some(20), // SHA-1
        3 => Some(32), // SHA-256
        4 => Some(8),  // CRC-64
        _ => None,
    }
}

fn piece_length_for(total_length: u64, num_pieces: usize) -> Result<u32> {
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

fn piece_count(total_length: u64, piece_length: u32) -> Result<usize> {
    if piece_length == 0 {
        return Err(Aria2Error::InvalidArgument(
            "Control file piece length must not be 0".to_string(),
        ));
    }
    usize::try_from(total_length.div_ceil(piece_length as u64))
        .map_err(|_| Aria2Error::InvalidArgument("Control file has too many pieces".to_string()))
}

fn validate_in_flight_pieces(
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

#[derive(Debug, Default)]
struct ProjectExtension {
    completed_length: Option<u64>,
    checksum_algo: u8,
    checksum_value: Vec<u8>,
    torrent_checkpoint: bool,
    torrent_info_hash: Option<[u8; 20]>,
    torrent_piece_length: Option<u32>,
}

fn serialize_project_extension(control_file: &ControlFile) -> Result<Vec<u8>> {
    if control_file.completed_length > control_file.total_length {
        return Err(Aria2Error::InvalidArgument(
            "Control file completed length exceeds total length".to_string(),
        ));
    }
    if (control_file.checksum_algo == 0 && !control_file.checksum_value.is_empty())
        || (control_file.checksum_algo != 0
            && checksum_length(control_file.checksum_algo)
                != Some(control_file.checksum_value.len()))
    {
        return Err(Aria2Error::InvalidArgument(format!(
            "Checksum length does not match algorithm {}",
            control_file.checksum_algo
        )));
    }
    if control_file
        .torrent_piece_length
        .is_some_and(|piece_length| piece_length == 0)
    {
        return Err(Aria2Error::InvalidArgument(
            "Control file torrent piece length must not be 0".to_string(),
        ));
    }

    let mut flags = EXT_HAS_COMPLETED_LENGTH;
    if control_file.checksum_algo != 0 {
        flags |= EXT_HAS_CHECKSUM;
    }
    if control_file.torrent_checkpoint {
        flags |= EXT_TORRENT_CHECKPOINT;
    }
    if control_file.torrent_info_hash.is_some() {
        flags |= EXT_HAS_INFO_HASH;
    }
    if control_file.torrent_piece_length.is_some() {
        flags |= EXT_HAS_PIECE_LENGTH;
    }

    let mut payload = Vec::with_capacity(8 + 1 + 4 + 32 + 20 + 4);
    payload.extend_from_slice(&control_file.completed_length.to_be_bytes());
    if flags & EXT_HAS_CHECKSUM != 0 {
        payload.push(control_file.checksum_algo);
        payload.extend_from_slice(&(control_file.checksum_value.len() as u32).to_be_bytes());
        payload.extend_from_slice(&control_file.checksum_value);
    }
    if let Some(info_hash) = control_file.torrent_info_hash {
        payload.extend_from_slice(&info_hash);
    }
    if let Some(piece_length) = control_file.torrent_piece_length {
        payload.extend_from_slice(&piece_length.to_be_bytes());
    }

    let payload_length = u32::try_from(payload.len()).map_err(|_| {
        Aria2Error::InvalidArgument("Control file extension payload is too large".to_string())
    })?;
    let mut extension = Vec::with_capacity(
        PROJECT_EXTENSION_HEADER_LEN + payload.len() + PROJECT_EXTENSION_DIGEST_LEN,
    );
    extension.extend_from_slice(PROJECT_EXTENSION_MAGIC);
    extension.extend_from_slice(&PROJECT_EXTENSION_VERSION.to_be_bytes());
    extension.extend_from_slice(&flags.to_be_bytes());
    extension.extend_from_slice(&payload_length.to_be_bytes());
    extension.extend_from_slice(&payload);
    let digest = Sha256::digest(&extension);
    extension.extend_from_slice(&digest);
    Ok(extension)
}

fn parse_project_extension(data: &[u8]) -> Result<Option<ProjectExtension>> {
    if data.is_empty() {
        return Ok(None);
    }
    if data.len() < PROJECT_EXTENSION_HEADER_LEN + PROJECT_EXTENSION_DIGEST_LEN
        || &data[..4] != PROJECT_EXTENSION_MAGIC
    {
        return Err(Aria2Error::FileIo(
            "Unexpected trailing bytes in native control file".to_string(),
        ));
    }

    let version = u16::from_be_bytes([data[4], data[5]]);
    if version != PROJECT_EXTENSION_VERSION {
        return Err(Aria2Error::FileIo(format!(
            "Unsupported control file extension version: {}",
            version
        )));
    }
    let flags = u16::from_be_bytes([data[6], data[7]]);
    if flags & !EXT_KNOWN_FLAGS != 0 {
        return Err(Aria2Error::FileIo(format!(
            "Unknown control file extension flags: 0x{:04x}",
            flags & !EXT_KNOWN_FLAGS
        )));
    }
    let payload_length =
        usize::try_from(u32::from_be_bytes([data[8], data[9], data[10], data[11]])).map_err(
            |_| Aria2Error::FileIo("Control file extension payload is too large".to_string()),
        )?;
    let payload_end = PROJECT_EXTENSION_HEADER_LEN
        .checked_add(payload_length)
        .ok_or_else(|| Aria2Error::FileIo("Control file extension length overflow".to_string()))?;
    let expected_length = payload_end
        .checked_add(PROJECT_EXTENSION_DIGEST_LEN)
        .ok_or_else(|| Aria2Error::FileIo("Control file extension length overflow".to_string()))?;
    if expected_length != data.len() {
        return Err(Aria2Error::FileIo(format!(
            "Control file extension length mismatch: declared {}, available {}",
            payload_length,
            data.len().saturating_sub(PROJECT_EXTENSION_HEADER_LEN)
        )));
    }
    let digest = Sha256::digest(&data[..payload_end]);
    if digest.as_slice() != &data[payload_end..] {
        return Err(Aria2Error::FileIo(
            "Control file extension checksum mismatch".to_string(),
        ));
    }

    let mut reader = NativeReader::payload(&data[PROJECT_EXTENSION_HEADER_LEN..payload_end]);
    let completed_length = if flags & EXT_HAS_COMPLETED_LENGTH != 0 {
        Some(reader.u64("extension completed length")?)
    } else {
        None
    };
    let (checksum_algo, checksum_value) = if flags & EXT_HAS_CHECKSUM != 0 {
        let checksum_algo = reader.bytes(1, "extension checksum algorithm")?[0];
        let value_length = usize::try_from(reader.u32("extension checksum length")?)
            .map_err(|_| Aria2Error::FileIo("Control file checksum is too large".to_string()))?;
        let expected_length = checksum_length(checksum_algo).ok_or_else(|| {
            Aria2Error::FileIo(format!(
                "Unsupported control file checksum algorithm: {}",
                checksum_algo
            ))
        })?;
        if value_length != expected_length {
            return Err(Aria2Error::FileIo(format!(
                "Control file checksum length mismatch: expected {}, actual {}",
                expected_length, value_length
            )));
        }
        (
            checksum_algo,
            reader
                .bytes(value_length, "extension checksum value")?
                .to_vec(),
        )
    } else {
        (0, Vec::new())
    };
    let torrent_checkpoint = flags & EXT_TORRENT_CHECKPOINT != 0;
    let torrent_info_hash = if flags & EXT_HAS_INFO_HASH != 0 {
        let mut info_hash = [0u8; 20];
        info_hash.copy_from_slice(reader.bytes(20, "extension info hash")?);
        Some(info_hash)
    } else {
        None
    };
    let torrent_piece_length = if flags & EXT_HAS_PIECE_LENGTH != 0 {
        let piece_length = reader.u32("extension piece length")?;
        if piece_length == 0 {
            return Err(Aria2Error::FileIo(
                "Control file extension piece length must not be 0".to_string(),
            ));
        }
        Some(piece_length)
    } else {
        None
    };
    if !reader.remaining().is_empty() {
        return Err(Aria2Error::FileIo(
            "Control file extension contains unexpected payload bytes".to_string(),
        ));
    }

    Ok(Some(ProjectExtension {
        completed_length,
        checksum_algo,
        checksum_value,
        torrent_checkpoint,
        torrent_info_hash,
        torrent_piece_length,
    }))
}

fn parse_native_control_file(path: &Path, data: &[u8]) -> Result<Option<ControlFile>> {
    let version = u16::from_be_bytes([data[0], data[1]]);
    let native = NativeReader::new(data, version == 1);
    let mut reader = native;
    // aria2 stores extension flags as a network-order byte mask even in v0;
    // only the following numeric fields use the host byte order in that version.
    let extension = reader.u32_be("extension")?;
    let is_torrent = extension & 1 != 0;
    let info_hash_length = usize::try_from(reader.u32("info hash length")?).map_err(|_| {
        Aria2Error::FileIo("Native control file info hash length is too large".to_string())
    })?;
    if info_hash_length > 20 || (is_torrent && info_hash_length != 20) {
        return Err(Aria2Error::FileIo(format!(
            "Invalid native control file info hash length: {}",
            info_hash_length
        )));
    }

    let torrent_info_hash = if info_hash_length == 20 {
        let bytes = reader.bytes(20, "info hash")?;
        let mut info_hash = [0u8; 20];
        info_hash.copy_from_slice(bytes);
        Some(info_hash)
    } else if info_hash_length != 0 {
        let _ = reader.bytes(info_hash_length, "info hash")?;
        None
    } else {
        None
    };

    let piece_length = reader.u32("piece length")?;
    if piece_length == 0 {
        return Err(Aria2Error::FileIo(
            "Native control file piece length must not be 0".to_string(),
        ));
    }
    let total_length = reader.u64("total length")?;
    let upload_length = reader.u64("upload length")?;
    let bitfield_length = usize::try_from(reader.u32("bitfield length")?).map_err(|_| {
        Aria2Error::FileIo("Native control file bitfield length is too large".to_string())
    })?;
    let num_pieces_u64 = if total_length == 0 {
        0
    } else {
        total_length.div_ceil(piece_length as u64)
    };
    let num_pieces = usize::try_from(num_pieces_u64)
        .map_err(|_| Aria2Error::FileIo("Native control file has too many pieces".to_string()))?;
    let expected_bitfield_length = num_pieces.div_ceil(8);
    if bitfield_length != expected_bitfield_length {
        return Err(Aria2Error::FileIo(format!(
            "Native control file bitfield length mismatch: expected {}, actual {}",
            expected_bitfield_length, bitfield_length
        )));
    }
    let bitfield = reader.bytes(bitfield_length, "bitfield")?.to_vec();
    if let Some(last_byte) = bitfield.last()
        && !num_pieces.is_multiple_of(8)
        && last_byte & (u8::MAX >> (num_pieces % 8)) != 0
    {
        return Err(Aria2Error::FileIo(
            "Native control file bitfield has set trailing bits".to_string(),
        ));
    }

    // Validate the in-flight-piece records even though the generic Rust
    // checkpoint does not currently restore block-level progress.  This
    // prevents malformed native files from being accepted as resumable state.
    let in_flight_count = usize::try_from(reader.u32("in-flight piece count")?).map_err(|_| {
        Aria2Error::FileIo("Native control file in-flight count is too large".to_string())
    })?;
    let mut in_flight_pieces = Vec::with_capacity(in_flight_count);
    for _ in 0..in_flight_count {
        let index = reader.u32("in-flight piece index")?;
        if index as usize >= num_pieces {
            return Err(Aria2Error::FileIo(format!(
                "Native control file piece index out of range: {}",
                index
            )));
        }
        let length = reader.u32("in-flight piece length")?;
        if length == 0 || length > piece_length {
            return Err(Aria2Error::FileIo(format!(
                "Native control file in-flight piece length out of range: {}",
                length
            )));
        }
        let block_bitfield_length = usize::try_from(reader.u32("in-flight bitfield length")?)
            .map_err(|_| {
                Aria2Error::FileIo(
                    "Native control file in-flight bitfield length is too large".to_string(),
                )
            })?;
        let expected_block_bitfield_length = (length as usize).div_ceil(16 * 1024).div_ceil(8);
        if block_bitfield_length != expected_block_bitfield_length {
            return Err(Aria2Error::FileIo(format!(
                "Native control file in-flight bitfield length mismatch: expected {}, actual {}",
                expected_block_bitfield_length, block_bitfield_length
            )));
        }
        let bitfield = reader
            .bytes(block_bitfield_length, "in-flight bitfield")?
            .to_vec();
        in_flight_pieces.push(ControlFileInFlightPiece {
            index,
            length,
            bitfield,
        });
    }
    validate_in_flight_pieces(
        &in_flight_pieces,
        total_length,
        piece_length,
        num_pieces,
        &bitfield,
    )?;

    let project_extension = parse_project_extension(reader.remaining())?;
    let mut completed_length =
        completed_length_from_bitfield(total_length, piece_length, &bitfield);
    let mut checksum_algo = 0;
    let mut checksum_value = Vec::new();
    let mut torrent_checkpoint = is_torrent;
    let mut final_info_hash = torrent_info_hash;
    let mut final_piece_length = is_torrent.then_some(piece_length);
    if let Some(extension) = project_extension {
        if let Some(value) = extension.completed_length {
            if value > total_length {
                return Err(Aria2Error::FileIo(
                    "Control file extension completed length exceeds total length".to_string(),
                ));
            }
            completed_length = value;
        }
        checksum_algo = extension.checksum_algo;
        checksum_value = extension.checksum_value;
        if extension.torrent_checkpoint && !is_torrent {
            return Err(Aria2Error::FileIo(
                "Control file extension marks a non-torrent native file as torrent state"
                    .to_string(),
            ));
        }
        if (extension.torrent_info_hash.is_some() || extension.torrent_piece_length.is_some())
            && !is_torrent
        {
            return Err(Aria2Error::FileIo(
                "Control file extension has torrent metadata without native torrent state"
                    .to_string(),
            ));
        }
        if let (Some(native), Some(extended)) = (final_info_hash, extension.torrent_info_hash)
            && native != extended
        {
            return Err(Aria2Error::FileIo(
                "Native and project torrent info hashes differ".to_string(),
            ));
        }
        if let (Some(native), Some(extended)) = (final_piece_length, extension.torrent_piece_length)
            && native != extended
        {
            return Err(Aria2Error::FileIo(
                "Native and project torrent piece lengths differ".to_string(),
            ));
        }
        torrent_checkpoint |= extension.torrent_checkpoint;
        final_info_hash = final_info_hash.or(extension.torrent_info_hash);
        final_piece_length = final_piece_length.or(extension.torrent_piece_length);
    }

    Ok(Some(ControlFile {
        path: path.to_path_buf(),
        total_length,
        completed_length,
        upload_length,
        bitfield,
        in_flight_pieces,
        num_pieces,
        checksum_algo,
        checksum_value,
        torrent_checkpoint,
        torrent_info_hash: final_info_hash,
        torrent_piece_length: final_piece_length,
        native_piece_length: Some(piece_length),
        native_layout: true,
    }))
}

struct NativeReader<'a> {
    data: &'a [u8],
    offset: usize,
    network_order: bool,
}

impl<'a> NativeReader<'a> {
    fn new(data: &'a [u8], network_order: bool) -> Self {
        Self {
            data,
            offset: 2,
            network_order,
        }
    }

    fn payload(data: &'a [u8]) -> Self {
        Self {
            data,
            offset: 0,
            network_order: true,
        }
    }

    fn remaining(&self) -> &'a [u8] {
        &self.data[self.offset..]
    }

    fn bytes(&mut self, length: usize, field: &str) -> Result<&'a [u8]> {
        let end = self.offset.checked_add(length).ok_or_else(|| {
            Aria2Error::FileIo(format!("Native control file {field} length overflow"))
        })?;
        if end > self.data.len() {
            return Err(Aria2Error::FileIo(format!(
                "Truncated native control file {field}"
            )));
        }
        let bytes = &self.data[self.offset..end];
        self.offset = end;
        Ok(bytes)
    }

    fn u32(&mut self, field: &str) -> Result<u32> {
        let bytes = self.bytes(4, field)?;
        Ok(if self.network_order {
            u32::from_be_bytes(bytes.try_into().expect("four-byte native field"))
        } else {
            u32::from_ne_bytes(bytes.try_into().expect("four-byte native field"))
        })
    }

    fn u32_be(&mut self, field: &str) -> Result<u32> {
        let bytes = self.bytes(4, field)?;
        Ok(u32::from_be_bytes(
            bytes.try_into().expect("four-byte native field"),
        ))
    }

    fn u64(&mut self, field: &str) -> Result<u64> {
        let bytes = self.bytes(8, field)?;
        Ok(if self.network_order {
            u64::from_be_bytes(bytes.try_into().expect("eight-byte native field"))
        } else {
            u64::from_ne_bytes(bytes.try_into().expect("eight-byte native field"))
        })
    }
}

fn completed_length_from_bitfield(total_length: u64, piece_length: u32, bitfield: &[u8]) -> u64 {
    if total_length == 0 || piece_length == 0 {
        return 0;
    }
    let num_pieces = total_length.div_ceil(piece_length as u64) as usize;
    (0..num_pieces)
        .filter(|index| {
            bitfield
                .get(index / 8)
                .is_some_and(|byte| byte & (1 << (7 - index % 8)) != 0)
        })
        .map(|index| {
            let offset = index as u64 * piece_length as u64;
            total_length.saturating_sub(offset).min(piece_length as u64)
        })
        .sum()
}

fn u16_from_le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn u32_from_le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn u64_from_le(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn original_aria2_bt_control_file_fixtures_restore_verified_progress() {
        const INFO_HASH: [u8; 20] = [
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
            0xff, 0x00, 0xff, 0xff, 0xff, 0xff,
        ];
        // Keep these upstream aria2 test fixtures in-tree so tests do not
        // depend on the locally ignored aria2_original checkout.
        const V0000_FIXTURE: [u8; 94] = [
            0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x14, 0x00, 0x00, 0x00, 0x11, 0x22, 0x33, 0x44,
            0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0xff, 0xff,
            0xff, 0xff, 0x00, 0x04, 0x00, 0x00, 0x00, 0x40, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x00, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe, 0x02, 0x00, 0x00, 0x00, 0x01, 0x00,
            0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00,
            0x00, 0x00, 0x02, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
        ];
        const V0001_FIXTURE: [u8; 94] = [
            0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x14, 0x11, 0x22, 0x33, 0x44,
            0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0xff, 0xff,
            0xff, 0xff, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x40, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x0a, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00,
            0x00, 0x01, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
            0x02, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
        ];
        let fixtures: [(&str, &[u8]); 2] = [("v0000", &V0000_FIXTURE), ("v0001", &V0001_FIXTURE)];
        let dir = tempfile::tempdir().unwrap();

        for (version, bytes) in fixtures {
            let path = dir.path().join(format!("original-{version}.aria2"));
            tokio::fs::write(&path, bytes).await.unwrap();
            let loaded = ControlFile::load(&path).await.unwrap().unwrap();

            assert!(loaded.uses_native_layout(), "version {version}");
            assert!(loaded.is_torrent_checkpoint(), "version {version}");
            assert_eq!(
                loaded.torrent_info_hash(),
                Some(INFO_HASH),
                "version {version}"
            );
            assert_eq!(loaded.piece_length(), Some(1024), "version {version}");
            assert_eq!(loaded.total_length(), 80 * 1024, "version {version}");
            assert_eq!(loaded.upload_length(), 1024, "version {version}");
            assert_eq!(loaded.completed_length(), 79 * 1024, "version {version}");
            assert_eq!(
                loaded.bitfield(),
                &[0xff; 9].into_iter().chain([0xfe]).collect::<Vec<_>>()
            );
            assert_eq!(
                loaded.in_flight_pieces(),
                &[
                    ControlFileInFlightPiece {
                        index: 1,
                        length: 1024,
                        bitfield: vec![0],
                    },
                    ControlFileInFlightPiece {
                        index: 2,
                        length: 512,
                        bitfield: vec![0],
                    },
                ],
                "version {version}"
            );
        }
    }

    #[tokio::test]
    async fn native_control_file_roundtrips_in_flight_block_bitfields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial-piece.bin.aria2");
        let mut control =
            ControlFile::open_or_create_with_piece_length(&path, 64 * 1024, 64 * 1024)
                .await
                .unwrap();
        control.mark_torrent_checkpoint();
        control.set_torrent_info_hash([0x31; 20]);
        control.set_in_flight_pieces(vec![ControlFileInFlightPiece {
            index: 0,
            length: 64 * 1024,
            bitfield: vec![0b1010_0000],
        }]);
        control.save().await.unwrap();

        let restored = ControlFile::load(&path).await.unwrap().unwrap();
        assert_eq!(restored.in_flight_pieces(), control.in_flight_pieces());
        assert_eq!(restored.bitfield(), &[0]);
    }

    #[test]
    fn test_control_path_uses_aria2_suffix() {
        assert_eq!(
            ControlFile::control_path_for(Path::new("payload.bin")),
            PathBuf::from("payload.bin.aria2")
        );
    }

    #[test]
    fn test_control_path_for_current_directory_is_a_sidecar_file() {
        let path = ControlFile::control_path_for(Path::new("."));
        assert_ne!(path, Path::new("."));
        assert_eq!(path.file_name(), Some(std::ffi::OsStr::new(".aria2")));
    }

    #[test]
    fn test_control_path_appends_after_multiple_extensions() {
        assert_eq!(
            ControlFile::control_path_for(Path::new("archive.tar.gz")),
            PathBuf::from("archive.tar.gz.aria2")
        );
    }

    #[test]
    fn test_control_path_for_multi_file_top_directory_is_a_sibling() {
        let path = ControlFile::control_path_for(Path::new("downloads/torrent"));
        assert_eq!(path, PathBuf::from("downloads/torrent.aria2"));
    }

    #[tokio::test]
    async fn test_control_file_new_and_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.aria2");

        let cf = ControlFile::open_or_create(&path, 10000, 10).await.unwrap();
        assert_eq!(cf.total_length(), 10000);
        assert_eq!(cf.completed_length(), 0);
        assert!(!cf.is_piece_done(0));

        cf.save().await.unwrap();

        assert!(path.exists());
        let data = tokio::fs::read(&path).await.unwrap();
        assert_eq!(&data[0..2], &1u16.to_be_bytes());
        assert_ne!(&data[0..4], CONTROL_MAGIC);
        assert!(
            data.windows(PROJECT_EXTENSION_MAGIC.len())
                .any(|window| { window == PROJECT_EXTENSION_MAGIC })
        );
    }

    #[tokio::test]
    async fn fixed_piece_checkpoint_persists_the_exact_piece_layout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixed-piece.bin.aria2");
        let total_length: u64 = 101 * 1024 * 1024;
        let piece_length = 2 * 1024 * 1024;
        let pieces = total_length.div_ceil(piece_length as u64) as usize;

        let checkpoint =
            ControlFile::open_or_create_with_piece_length(&path, total_length, piece_length)
                .await
                .unwrap();
        assert_eq!(checkpoint.piece_length(), Some(piece_length));
        assert_eq!(checkpoint.bitfield().len(), pieces.div_ceil(8));
        checkpoint.save().await.unwrap();

        let loaded = ControlFile::load(&path).await.unwrap().unwrap();
        assert_eq!(loaded.piece_length(), Some(piece_length));
        assert_eq!(loaded.bitfield().len(), pieces.div_ceil(8));
        assert!(
            ControlFile::open_or_create_with_piece_length(&path, total_length, piece_length / 2,)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_loads_native_aria2_v1_control_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("native.bin.aria2");
        let mut data = Vec::new();
        data.extend_from_slice(&1u16.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(&2u32.to_be_bytes());
        data.extend_from_slice(&3u64.to_be_bytes());
        data.extend_from_slice(&7u64.to_be_bytes());
        data.extend_from_slice(&1u32.to_be_bytes());
        data.push(0b1100_0000);
        data.extend_from_slice(&0u32.to_be_bytes());
        tokio::fs::write(&path, data).await.unwrap();

        let loaded = ControlFile::load(&path).await.unwrap().unwrap();
        assert_eq!(loaded.total_length(), 3);
        assert_eq!(loaded.upload_length(), 7);
        assert_eq!(loaded.completed_length(), 3);
        assert_eq!(loaded.bitfield(), &[0b1100_0000]);
        assert!(!loaded.is_torrent_checkpoint());
    }

    #[tokio::test]
    async fn test_loads_native_aria2_v0_control_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("native-v0.bin.aria2");
        let mut data = Vec::new();
        data.extend_from_slice(&0u16.to_ne_bytes());
        data.extend_from_slice(&0u32.to_ne_bytes());
        data.extend_from_slice(&0u32.to_ne_bytes());
        data.extend_from_slice(&4u32.to_ne_bytes());
        data.extend_from_slice(&4u64.to_ne_bytes());
        data.extend_from_slice(&0u64.to_ne_bytes());
        data.extend_from_slice(&1u32.to_ne_bytes());
        data.push(0b1000_0000);
        data.extend_from_slice(&0u32.to_ne_bytes());
        tokio::fs::write(&path, data).await.unwrap();

        let loaded = ControlFile::load(&path).await.unwrap().unwrap();
        assert_eq!(loaded.total_length(), 4);
        assert_eq!(loaded.completed_length(), 4);
    }

    #[tokio::test]
    async fn test_control_file_mark_and_check_pieces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.aria2");

        let mut cf = ControlFile::open_or_create(&path, 1000, 8).await.unwrap();

        cf.mark_piece_done(0);
        cf.mark_piece_done(3);
        cf.mark_piece_done(7);

        assert!(cf.is_piece_done(0));
        assert!(!cf.is_piece_done(1));
        assert!(cf.is_piece_done(3));
        assert!(!cf.is_piece_done(5));
        assert!(cf.is_piece_done(7));
        assert_eq!(cf.completed_pieces(), 3);

        cf.save().await.unwrap();

        let loaded = ControlFile::load(&path).await.unwrap().unwrap();
        assert_eq!(loaded.completed_pieces(), 3);
        assert!(loaded.is_piece_done(0));
        assert!(loaded.is_piece_done(7));
    }

    #[tokio::test]
    async fn test_control_file_piece_completion_handles_short_final_piece() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short_final.aria2");

        let mut cf = ControlFile::open_or_create(&path, 10, 3).await.unwrap();
        cf.mark_piece_done(0);
        cf.mark_piece_done(2);

        assert_eq!(cf.completed_length(), 6);
        assert!(!cf.is_piece_done(3));
    }

    #[tokio::test]
    async fn test_control_file_reload_restores_logical_piece_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reload_piece_count.aria2");

        let mut cf = ControlFile::open_or_create(&path, 10, 3).await.unwrap();
        cf.mark_piece_done(0);
        cf.save().await.unwrap();

        let mut loaded = ControlFile::open_or_create(&path, 10, 3).await.unwrap();
        loaded.mark_piece_done(2);

        assert_eq!(loaded.completed_length(), 6);
        assert!(!loaded.is_piece_done(3));
    }

    #[tokio::test]
    async fn test_control_file_reload_normalizes_legacy_piece_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("normalize_piece_count.aria2");

        // This is the retired A2CF layout. It intentionally exercises the
        // compatibility reader instead of the current official writer.
        let mut legacy = Vec::new();
        legacy.extend_from_slice(CONTROL_MAGIC);
        legacy.extend_from_slice(&CONTROL_VERSION.to_le_bytes());
        legacy.push(0);
        legacy.extend_from_slice(&10u64.to_le_bytes());
        legacy.extend_from_slice(&6u64.to_le_bytes());
        legacy.extend_from_slice(&0u64.to_le_bytes());
        legacy.extend_from_slice(&1u64.to_le_bytes());
        legacy.push(0b1100_1000);
        tokio::fs::write(&path, legacy).await.unwrap();

        let loaded = ControlFile::open_or_create(&path, 10, 3).await.unwrap();

        assert_eq!(loaded.bitfield(), &[0b1100_0000]);
        assert_eq!(loaded.completed_pieces(), 2);
        assert!(loaded.is_piece_done(0));
        assert!(loaded.is_piece_done(1));
        assert!(!loaded.is_piece_done(2));
        assert!(!loaded.is_piece_done(3));
        assert_eq!(loaded.completed_length(), 8);
    }

    #[tokio::test]
    async fn test_control_file_roundtrip_with_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_hash.aria2");

        let mut cf = ControlFile::open_or_create(&path, 5000, 5).await.unwrap();
        cf.checksum_algo = 2;
        cf.checksum_value = vec![0xAB; 20];
        cf.mark_piece_done(0);
        cf.mark_piece_done(2);
        cf.save().await.unwrap();

        let loaded = ControlFile::load(&path).await.unwrap().unwrap();
        assert_eq!(loaded.total_length(), 5000);
        assert_eq!(loaded.checksum_algo, 2);
        assert_eq!(loaded.completed_pieces(), 2);
    }

    #[tokio::test]
    async fn test_control_file_rejects_corrupt_project_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt_extension.aria2");
        let control = ControlFile::open_or_create(&path, 100, 4).await.unwrap();
        control.save().await.unwrap();

        let mut data = tokio::fs::read(&path).await.unwrap();
        *data.last_mut().unwrap() ^= 0x01;
        tokio::fs::write(&path, data).await.unwrap();

        assert!(ControlFile::load(&path).await.is_err());
    }

    #[tokio::test]
    async fn test_control_file_atomic_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_atomic.aria2");

        let mut cf = ControlFile::open_or_create(&path, 999, 4).await.unwrap();
        cf.mark_piece_done(1);
        cf.save().await.unwrap();

        let tmp_path = path.with_extension("aria2.tmp");
        assert!(!tmp_path.exists());
        assert!(path.exists());
    }

    #[tokio::test]
    async fn test_control_file_load_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.aria2");
        let result = ControlFile::load(&path).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_control_file_load_invalid_magic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.aria2");
        tokio::fs::write(&path, b"NOT_A2CF_DATA").await.unwrap();

        let result = ControlFile::load(&path).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_control_file_load_truncated_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.aria2");
        for length in [0usize, 7, 8, 38] {
            let mut data = vec![0u8; length];
            if length >= 4 {
                data[..4].copy_from_slice(CONTROL_MAGIC);
            }
            tokio::fs::write(&path, &data).await.unwrap();
            assert!(ControlFile::load(&path).await.is_err(), "length={length}");
        }
    }

    #[tokio::test]
    async fn test_control_file_rejects_invalid_checksum_and_bitfield_lengths() {
        let dir = tempfile::tempdir().expect("failed to create temporary directory");
        let path = dir.path().join("malformed.aria2");
        let mut data = vec![0u8; CONTROL_HEADER_LEN];
        data[..4].copy_from_slice(CONTROL_MAGIC);
        data[6] = FLAG_HAS_CHECKSUM;
        data[31..39].copy_from_slice(&0u64.to_le_bytes());
        data.push(2);
        tokio::fs::write(&path, &data).await.unwrap();
        assert!(ControlFile::load(&path).await.is_err());

        let mut data = vec![0u8; CONTROL_HEADER_LEN];
        data[..4].copy_from_slice(CONTROL_MAGIC);
        data[31..39].copy_from_slice(&1u64.to_le_bytes());
        tokio::fs::write(&path, &data).await.unwrap();
        assert!(ControlFile::load(&path).await.is_err());
    }

    #[tokio::test]
    async fn test_control_path_for_output() {
        let out = Path::new("/downloads/file.iso");
        let ctrl = ControlFile::control_path_for(out);
        assert_eq!(ctrl.extension().unwrap().to_str().unwrap(), "aria2");
        assert_eq!(ctrl, PathBuf::from("/downloads/file.iso.aria2"));
    }

    #[tokio::test]
    async fn test_control_file_update_completed_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_len.aria2");

        let mut cf = ControlFile::open_or_create(&path, 8000, 8).await.unwrap();
        cf.update_completed_length(3500);
        assert_eq!(cf.completed_length(), 3500);

        cf.update_completed_length(9000);
        assert_eq!(cf.completed_length(), 8000);

        cf.update_completed_length(3500);
        cf.save().await.unwrap();
        let loaded = ControlFile::load(&path).await.unwrap().unwrap();
        assert_eq!(loaded.completed_length(), 3500);
    }
}
