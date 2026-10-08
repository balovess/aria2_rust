use super::{ControlFile, ControlFileInFlightPiece, checksum_length, validate_in_flight_pieces};
use crate::error::{Aria2Error, Result};
use sha2::{Digest, Sha256};
use std::path::Path;

// The prefix of every newly-written file is the official aria2 v1 format.
// A2RX is only the project's extension trailer; aria2's reader stops after
// the official in-flight records and therefore remains compatible with it.
pub(super) const PROJECT_EXTENSION_MAGIC: &[u8; 4] = b"A2RX";
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

#[derive(Debug, Default)]
struct ProjectExtension {
    completed_length: Option<u64>,
    checksum_algo: u8,
    checksum_value: Vec<u8>,
    torrent_checkpoint: bool,
    torrent_info_hash: Option<[u8; 20]>,
    torrent_piece_length: Option<u32>,
}

pub(super) fn serialize_project_extension(control_file: &ControlFile) -> Result<Vec<u8>> {
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

pub(super) fn parse_native_control_file(path: &Path, data: &[u8]) -> Result<Option<ControlFile>> {
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
