//! Text format deserialization for BT progress info files.
//!
//! Supports backward compatibility with C++ aria2 legacy INI text `.aria2`
//! files. The text format stores all fields including statistics that are
//! not present in the binary format.

use std::time::SystemTime;

use crate::error::{Aria2Error, Result};

use super::types::{BtProgress, PeerAddr, hex_to_info_hash};

/// Deserialize progress from legacy INI text format.
///
/// This supports backward compatibility with C++ aria2 `.aria2` files.
/// The text format includes statistics fields (downloaded, uploaded,
/// elapsed) that are not present in the binary format.
///
/// # Peer address parsing
///
/// Peer lines are in `ip:port` format (e.g., `192.168.1.1:6881`).
/// Using `rsplitn(2, ':')` splits from the right, correctly handling
/// IPv6 addresses with colons in the IP portion.
pub fn deserialize_text(data: &[u8], info_hash: &[u8; 20]) -> Result<BtProgress> {
    let text = String::from_utf8_lossy(data);
    let mut progress = BtProgress {
        info_hash: *info_hash,
        ..Default::default()
    };

    // Track whether the file contained an info_hash line so we can
    // validate it. A corrupted info_hash (wrong length, invalid hex, or
    // mismatch with expected) must result in an error.
    let mut info_hash_found = false;
    let mut info_hash_valid = false;

    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("info_hash=") {
            info_hash_found = true;
            // Parse hex info hash — must be exactly 40 hex chars
            if rest.len() == 40
                && let Ok(hash) = hex_to_info_hash(rest)
                && &hash == info_hash
            {
                info_hash_valid = true;
                progress.info_hash = hash;
            }
        } else if let Some(rest) = line.strip_prefix("version=") {
            if let Ok(v) = rest.parse::<u32>() {
                progress.version = v;
            }
        } else if let Some(rest) = line.strip_prefix("num_pieces=") {
            if let Ok(v) = rest.parse::<u32>() {
                progress.num_pieces = v;
            }
        } else if let Some(rest) = line.strip_prefix("piece_length=") {
            if let Ok(v) = rest.parse::<u32>() {
                progress.piece_length = v;
            }
        } else if let Some(rest) = line.strip_prefix("total_size=") {
            if let Ok(v) = rest.parse::<u64>() {
                progress.total_size = v;
            }
        } else if let Some(rest) = line.strip_prefix("downloaded=") {
            if let Ok(v) = rest.parse::<u64>() {
                progress.stats.downloaded_bytes = v;
            }
        } else if let Some(rest) = line.strip_prefix("uploaded=") {
            if let Ok(v) = rest.parse::<u64>() {
                progress.upload_length = v;
                progress.stats.uploaded_bytes = v;
            }
        } else if let Some(rest) = line.strip_prefix("elapsed=") {
            if let Ok(v) = rest.parse::<u64>() {
                progress.stats.elapsed_seconds = v;
            }
        } else if let Some(rest) = line.strip_prefix("bitfield=") {
            // Parse hex bitfield
            let bf_bytes: Vec<u8> = (0..rest.len())
                .step_by(2)
                .filter_map(|i| {
                    if i + 2 <= rest.len() {
                        u8::from_str_radix(&rest[i..i + 2], 16).ok()
                    } else {
                        None
                    }
                })
                .collect();
            progress.bitfield = bf_bytes;
        } else if line.contains(':') && !line.starts_with('[') {
            // Parse peer address (ip:port)
            // rsplitn(2, ':') splits from the right, yielding [port, ip]
            // This correctly handles IPv6 addresses with colons in the IP.
            let parts: Vec<&str> = line.rsplitn(2, ':').collect();
            if parts.len() == 2 {
                // parts[0] = port (after rightmost ':')
                // parts[1] = ip (before rightmost ':')
                if let Ok(port) = parts[0].parse::<u16>() {
                    progress.peers.push(PeerAddr {
                        ip: parts[1].to_string(),
                        port,
                    });
                }
            }
        }
    }

    progress.is_torrent = true;
    progress.save_time = SystemTime::now();

    // Validate info_hash: if the file contained an info_hash line but it
    // was corrupted (wrong length, invalid hex, or mismatch with expected),
    // the file is considered corrupted and we return an error.
    if info_hash_found && !info_hash_valid {
        return Err(Aria2Error::InvalidArgument(
            "Corrupted info_hash in progress file".to_string(),
        ));
    }

    Ok(progress)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_format_basic() {
        let info_hash = [0x22; 20];
        let hex_hash: String = info_hash.iter().map(|b| format!("{:02x}", b)).collect();

        let text_content = format!(
            "[Download]\n\
             info_hash={}\n\
             version=1\n\
             num_pieces=4\n\
             piece_length=262144\n\
             total_size=1048576\n\
             downloaded=524288\n\
             uploaded=262144\n\
             elapsed=60\n\
             bitfield=f0\n",
            hex_hash
        );

        let progress =
            deserialize_text(text_content.as_bytes(), &info_hash).expect("deserialize failed");

        assert_eq!(progress.piece_length, 262144);
        assert_eq!(progress.total_size, 1048576);
        assert_eq!(progress.num_pieces, 4);
        assert_eq!(progress.upload_length, 262144);
        assert_eq!(progress.stats.downloaded_bytes, 524288);
        assert_eq!(progress.stats.uploaded_bytes, 262144);
        assert_eq!(progress.stats.elapsed_seconds, 60);
        assert_eq!(progress.bitfield, vec![0xF0]);
    }

    #[test]
    fn test_text_format_with_peers() {
        let info_hash = [0x33; 20];
        let hex_hash: String = info_hash.iter().map(|b| format!("{:02x}", b)).collect();

        let text_content = format!(
            "[Download]\n\
             info_hash={}\n\
             piece_length=1024\n\
             total_size=4096\n\
             bitfield=ff\n\
             [Peers]\n\
             192.168.1.1:6881\n\
             10.0.0.1:6889\n",
            hex_hash
        );

        let progress =
            deserialize_text(text_content.as_bytes(), &info_hash).expect("deserialize failed");

        assert_eq!(progress.peers.len(), 2);
        assert_eq!(progress.peers[0].ip, "192.168.1.1");
        assert_eq!(progress.peers[0].port, 6881);
        assert_eq!(progress.peers[1].ip, "10.0.0.1");
        assert_eq!(progress.peers[1].port, 6889);
    }

    #[test]
    fn test_text_format_corrupted_info_hash() {
        let info_hash = [0x44; 20];

        let text_content = "[Download]\ninfo_hash=invalid_hex\nversion=1\n";
        let result = deserialize_text(text_content.as_bytes(), &info_hash);
        assert!(result.is_err(), "Corrupted info_hash should return error");
    }
}
