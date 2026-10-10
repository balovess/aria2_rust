use super::native::{parse_native_control_file, serialize_project_extension};
use super::{ControlFile, checksum_length, piece_count, validate_in_flight_pieces};
use crate::error::{Aria2Error, Result};
use std::path::Path;
use tokio::io::AsyncWriteExt;

// A2CF was the pre-release Rust-only write format. Keep its reader for
// migration compatibility, but do not add new fields to it: it will be
// removed after old checkpoints have aged out.
pub(super) const CONTROL_MAGIC: &[u8; 4] = b"A2CF";
pub(super) const CONTROL_VERSION: u16 = 1;
pub(super) const FLAG_HAS_CHECKSUM: u8 = 0x01;
pub(super) const FLAG_TORRENT_CHECKPOINT: u8 = 0x02;
pub(super) const FLAG_TORRENT_INFO_HASH: u8 = 0x04;
pub(super) const FLAG_TORRENT_PIECE_LENGTH: u8 = 0x08;
pub(super) const CONTROL_HEADER_LEN: usize = 39;

impl ControlFile {
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
        // The staged checkpoint's bytes and metadata must be durable before
        // its name can become the checkpoint visible to resume.
        file.sync_all()
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        drop(file);
        // Persist the rename itself as well: POSIX synchronizes the containing
        // directory chain; Windows uses MOVEFILE_WRITE_THROUGH.
        crate::filesystem::durability::replace_file(&tmp_path, &self.path)
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        Ok(())
    }
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
