//! Binary serialization/deserialization for BT progress info files.
//!
//! Implements the binary format compatible with C++ aria2's
//! `DefaultBtProgressInfoFile`. All multi-byte integers are stored in
//! network byte order (big-endian) as per C++ version 0001.
//!
//! # Binary format layout (version 0001)
//!
//! ```text
//! version          — 2 bytes  [0x00, 0x01]
//! extension        — 4 bytes  BE (bit0=1 for BT)
//! infoHashLength   — 4 bytes  BE (20 for BT, 0 for non-BT)
//! infoHash         — 20 bytes (present if infoHashLength == 20)
//! pieceLength      — 4 bytes  BE
//! totalLength      — 8 bytes  BE (int64_t)
//! uploadLength     — 8 bytes  BE (int64_t)
//! bitfieldLength   — 4 bytes  BE
//! bitfield         — N bytes
//! numInFlightPiece — 4 bytes  BE
//! for each in-flight piece:
//!   index          — 4 bytes  BE
//!   length         — 4 bytes  BE
//!   bitfieldLength — 4 bytes  BE
//!   bitfield       — N bytes
//! ```

use std::time::SystemTime;

use tracing::debug;

use crate::error::{Aria2Error, Result};

use super::types::{BtProgress, DownloadStats, InFlightPiece};

/// Extension bit flag: bit0 indicates a BitTorrent download.
const EXT_BT: u32 = 0x0000_0001;

/// Info hash length for BitTorrent downloads (SHA-1 = 20 bytes).
const INFO_HASH_LENGTH: u32 = 20;

// ===========================================================================
// Serialization
// ===========================================================================

/// Serialize progress to binary format (network byte order / big-endian).
///
/// The layout matches C++ `DefaultBtProgressInfoFile::save()` version 0001.
/// Stats fields (downloaded_bytes, uploaded_bytes, elapsed_seconds) are NOT
/// included in the binary format — they are only persisted in text format.
pub fn serialize_binary(progress: &BtProgress) -> Result<Vec<u8>> {
    let mut buf = Vec::new();

    // Version: 2 bytes [0x00, 0x01] (matches C++)
    buf.extend_from_slice(&[0x00, 0x01]);

    // Extension: 4 bytes BE (bit0=1 for BT)
    let extension: u32 = if progress.is_torrent { EXT_BT } else { 0 };
    buf.extend_from_slice(&extension.to_be_bytes());

    // infoHashLength: 4 bytes BE (20 for BT, 0 for non-BT)
    let info_hash_len: u32 = if progress.is_torrent {
        INFO_HASH_LENGTH
    } else {
        0
    };
    buf.extend_from_slice(&info_hash_len.to_be_bytes());

    // infoHash: 20 bytes (only if BT)
    if progress.is_torrent {
        buf.extend_from_slice(&progress.info_hash);
    }

    // pieceLength: 4 bytes BE
    buf.extend_from_slice(&progress.piece_length.to_be_bytes());

    // totalLength: 8 bytes BE (int64_t)
    buf.extend_from_slice(&progress.total_size.to_be_bytes());

    // uploadLength: 8 bytes BE (int64_t)
    buf.extend_from_slice(&progress.upload_length.to_be_bytes());

    // bitfieldLength: 4 bytes BE
    buf.extend_from_slice(&(progress.bitfield.len() as u32).to_be_bytes());

    // bitfield: N bytes
    buf.extend_from_slice(&progress.bitfield);

    // numInFlightPiece: 4 bytes BE
    buf.extend_from_slice(&(progress.in_flight_pieces.len() as u32).to_be_bytes());

    // Per in-flight piece
    for piece in &progress.in_flight_pieces {
        // index: 4 bytes BE
        buf.extend_from_slice(&piece.index.to_be_bytes());
        // length: 4 bytes BE
        buf.extend_from_slice(&piece.length.to_be_bytes());
        // bitfieldLength: 4 bytes BE
        buf.extend_from_slice(&(piece.bitfield.len() as u32).to_be_bytes());
        // bitfield: N bytes
        buf.extend_from_slice(&piece.bitfield);
    }

    Ok(buf)
}

// ===========================================================================
// Deserialization
// ===========================================================================

/// Deserialize progress from binary format (network byte order / big-endian).
///
/// Validates the stored info_hash against the expected info_hash, matching
/// C++ behavior. Returns an error if they don't match.
pub fn deserialize_binary(data: &[u8], expected_info_hash: &[u8; 20]) -> Result<BtProgress> {
    // Minimum size: version(2) + extension(4) + infoHashLength(4) +
    //                pieceLength(4) + totalLength(8) + uploadLength(8) +
    //                bitfieldLength(4) + numInFlightPiece(4)
    let min_size = 2 + 4 + 4 + 4 + 8 + 8 + 4 + 4;
    if data.len() < min_size {
        return Err(Aria2Error::InvalidArgument(
            "Binary progress file too short".to_string(),
        ));
    }

    let mut pos = 0;

    // Version: 2 bytes
    let version_hi = data[pos];
    let version_lo = data[pos + 1];
    pos += 2;

    let version = ((version_hi as u32) << 8) | (version_lo as u32);
    if version != 1 {
        return Err(Aria2Error::InvalidArgument(format!(
            "Unsupported binary progress file version: {}",
            version
        )));
    }

    // Extension: 4 bytes BE
    let extension = read_u32_be(data, &mut pos)?;
    let is_torrent = (extension & EXT_BT) != 0;

    // infoHashLength: 4 bytes BE
    let info_hash_len = read_u32_be(data, &mut pos)? as usize;

    // Validate infoHashLength
    if info_hash_len > INFO_HASH_LENGTH as usize {
        return Err(Aria2Error::InvalidArgument(format!(
            "Invalid info hash length: {}",
            info_hash_len
        )));
    }

    // If BT extension is set, infoHashLength must be 20
    if is_torrent && info_hash_len != INFO_HASH_LENGTH as usize {
        return Err(Aria2Error::InvalidArgument(format!(
            "BT download requires info hash length {}, got {}",
            INFO_HASH_LENGTH, info_hash_len
        )));
    }

    // Read infoHash if present
    let mut info_hash = [0u8; 20];
    if info_hash_len > 0 {
        if pos + info_hash_len > data.len() {
            return Err(Aria2Error::InvalidArgument(
                "Binary progress file truncated (info hash)".to_string(),
            ));
        }
        info_hash[..info_hash_len].copy_from_slice(&data[pos..pos + info_hash_len]);
        pos += info_hash_len;

        // Validate info_hash matches expected (matches C++ behavior)
        if is_torrent && &info_hash != expected_info_hash {
            let expected_hex: String = expected_info_hash
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect();
            let actual_hex: String = info_hash.iter().map(|b| format!("{:02x}", b)).collect();
            return Err(Aria2Error::InvalidArgument(format!(
                "info hash mismatch. expected: {}, actual: {}",
                expected_hex, actual_hex
            )));
        }
        debug!("Binary progress file info_hash validated successfully");
    }

    // pieceLength: 4 bytes BE
    let piece_length = read_u32_be(data, &mut pos)?;
    if piece_length == 0 {
        return Err(Aria2Error::InvalidArgument(
            "piece length must not be 0".to_string(),
        ));
    }

    // totalLength: 8 bytes BE
    let total_size = read_u64_be(data, &mut pos)?;

    // uploadLength: 8 bytes BE
    let upload_length = read_u64_be(data, &mut pos)?;

    // bitfieldLength: 4 bytes BE
    let bf_len = read_u32_be(data, &mut pos)? as usize;
    if pos + bf_len > data.len() {
        return Err(Aria2Error::InvalidArgument(
            "Binary progress file truncated (bitfield)".to_string(),
        ));
    }
    let bitfield = data[pos..pos + bf_len].to_vec();
    pos += bf_len;

    // Compute num_pieces from bitfield
    let num_pieces = total_size.div_ceil(piece_length as u64);

    // numInFlightPiece: 4 bytes BE
    let num_in_flight = read_u32_be(data, &mut pos)?;
    let mut in_flight_pieces = Vec::with_capacity(num_in_flight as usize);
    for _ in 0..num_in_flight {
        let index = read_u32_be(data, &mut pos)?;
        let length = read_u32_be(data, &mut pos)?;
        let inner_bf_len = read_u32_be(data, &mut pos)? as usize;
        if pos + inner_bf_len > data.len() {
            return Err(Aria2Error::InvalidArgument(
                "Binary progress file truncated (in-flight bitfield)".to_string(),
            ));
        }
        let piece_bf = data[pos..pos + inner_bf_len].to_vec();
        pos += inner_bf_len;
        in_flight_pieces.push(InFlightPiece::new(index, length, piece_bf));
    }

    Ok(BtProgress {
        info_hash: if is_torrent {
            info_hash
        } else {
            *expected_info_hash
        },
        bitfield,
        peers: Vec::new(), // Binary format does not persist peers
        stats: DownloadStats {
            // C++ restores uploadLength into the runtime stats via
            // btRuntime_->setUploadLengthAtStartup(uploadLength).
            // Mirror this by setting uploaded_bytes from the persisted field.
            uploaded_bytes: upload_length,
            ..DownloadStats::default()
        },
        piece_length,
        total_size,
        num_pieces: num_pieces as u32,
        upload_length,
        in_flight_pieces,
        is_torrent,
        save_time: SystemTime::now(),
        version,
    })
}

// ===========================================================================
// Helper functions for reading big-endian integers
// ===========================================================================

/// Read a big-endian u32 from the data at the given position.
fn read_u32_be(data: &[u8], pos: &mut usize) -> Result<u32> {
    if *pos + 4 > data.len() {
        return Err(Aria2Error::InvalidArgument(
            "Binary progress file truncated (u32)".to_string(),
        ));
    }
    let value = u32::from_be_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]);
    *pos += 4;
    Ok(value)
}

/// Read a big-endian u64 from the data at the given position.
fn read_u64_be(data: &[u8], pos: &mut usize) -> Result<u64> {
    if *pos + 8 > data.len() {
        return Err(Aria2Error::InvalidArgument(
            "Binary progress file truncated (u64)".to_string(),
        ));
    }
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&data[*pos..*pos + 8]);
    let value = u64::from_be_bytes(bytes);
    *pos += 8;
    Ok(value)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_binary_roundtrip() {
        let info_hash = [0xAA; 20];
        let progress = BtProgress {
            info_hash,
            bitfield: vec![0xFF, 0x0F],
            peers: Vec::new(),
            stats: DownloadStats::default(),
            piece_length: 262144,
            total_size: 524288,
            num_pieces: 2,
            upload_length: 1024,
            in_flight_pieces: vec![InFlightPiece::new(0, 16384, vec![0xFF])],
            is_torrent: true,
            save_time: SystemTime::now(),
            version: 1,
        };

        let data = serialize_binary(&progress).expect("serialize failed");
        let loaded = deserialize_binary(&data, &info_hash).expect("deserialize failed");

        assert_eq!(loaded.info_hash, info_hash);
        assert_eq!(loaded.bitfield, progress.bitfield);
        assert_eq!(loaded.piece_length, progress.piece_length);
        assert_eq!(loaded.total_size, progress.total_size);
        assert_eq!(loaded.upload_length, progress.upload_length);
        // C++ restores uploadLength into the runtime stats.
        // Our deserialize mirrors this by setting uploaded_bytes from upload_length.
        assert_eq!(loaded.stats.uploaded_bytes, progress.upload_length);
        assert_eq!(loaded.in_flight_pieces.len(), 1);
        assert_eq!(loaded.in_flight_pieces[0].index, 0);
        assert_eq!(loaded.in_flight_pieces[0].length, 16384);
        assert_eq!(loaded.in_flight_pieces[0].bitfield, vec![0xFF]);
    }

    #[test]
    fn test_binary_info_hash_mismatch() {
        let info_hash = [0xAA; 20];
        let progress = BtProgress {
            info_hash,
            bitfield: vec![0xFF],
            peers: Vec::new(),
            stats: DownloadStats::default(),
            piece_length: 262144,
            total_size: 262144,
            num_pieces: 1,
            upload_length: 0,
            in_flight_pieces: Vec::new(),
            is_torrent: true,
            save_time: SystemTime::now(),
            version: 1,
        };

        let data = serialize_binary(&progress).expect("serialize failed");
        let wrong_hash = [0xBB; 20];
        let result = deserialize_binary(&data, &wrong_hash);
        assert!(result.is_err(), "info hash mismatch should return error");
    }

    #[test]
    fn test_binary_non_bt_format() {
        let info_hash = [0x00; 20];
        let progress = BtProgress {
            info_hash,
            bitfield: vec![0xFF],
            peers: Vec::new(),
            stats: DownloadStats::default(),
            piece_length: 262144,
            total_size: 262144,
            num_pieces: 1,
            upload_length: 0,
            in_flight_pieces: Vec::new(),
            is_torrent: false,
            save_time: SystemTime::now(),
            version: 1,
        };

        let data = serialize_binary(&progress).expect("serialize failed");
        let loaded = deserialize_binary(&data, &info_hash).expect("deserialize failed");

        assert!(!loaded.is_torrent);
        assert_eq!(loaded.piece_length, 262144);
        assert_eq!(loaded.total_size, 262144);
    }

    #[test]
    fn test_binary_endianness() {
        // Verify that the serialized format uses big-endian.
        // Write a known value and check the raw bytes.
        let info_hash = [0x11; 20];
        let progress = BtProgress {
            info_hash,
            bitfield: vec![0xF0],
            peers: Vec::new(),
            stats: DownloadStats::default(),
            piece_length: 0x01020304, // known value for BE check
            total_size: 0,
            num_pieces: 1,
            upload_length: 0,
            in_flight_pieces: Vec::new(),
            is_torrent: true,
            save_time: SystemTime::now(),
            version: 1,
        };

        let data = serialize_binary(&progress).expect("serialize failed");

        // After version(2) + extension(4) + infoHashLength(4) + infoHash(20) = offset 30
        // pieceLength should be at offset 30 as BE u32
        let offset = 2 + 4 + 4 + 20;
        assert_eq!(data[offset], 0x01);
        assert_eq!(data[offset + 1], 0x02);
        assert_eq!(data[offset + 2], 0x03);
        assert_eq!(data[offset + 3], 0x04);
    }

    #[test]
    fn test_binary_extension_bit() {
        let info_hash = [0x22; 20];

        // BT download: extension should have bit0 set
        let bt_progress = BtProgress {
            info_hash,
            bitfield: vec![0xFF],
            peers: Vec::new(),
            stats: DownloadStats::default(),
            piece_length: 1024,
            total_size: 1024,
            num_pieces: 1,
            upload_length: 0,
            in_flight_pieces: Vec::new(),
            is_torrent: true,
            save_time: SystemTime::now(),
            version: 1,
        };
        let data = serialize_binary(&bt_progress).expect("serialize failed");
        // extension at offset 2, 4 bytes BE
        assert_eq!(data[2], 0x00);
        assert_eq!(data[3], 0x00);
        assert_eq!(data[4], 0x00);
        assert_eq!(data[5], 0x01); // bit0 = BT

        // Non-BT download: extension should be 0
        let non_bt_progress = BtProgress {
            is_torrent: false,
            ..bt_progress.clone()
        };
        let data = serialize_binary(&non_bt_progress).expect("serialize failed");
        assert_eq!(data[2..6], [0x00, 0x00, 0x00, 0x00]);
    }
}
