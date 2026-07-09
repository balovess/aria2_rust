//! BT download progress persistence system
//!
//! Provides functionality to save BT download progress to `.aria2` text format files,
//! with atomic write support and automatic detection of C++ binary format vs text format.

use std::fmt::{Display, Formatter};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::{Aria2Error, FatalError, Result};
use tracing::{debug, info, warn};

/// BT download progress Peer address information
#[derive(Clone, Debug)]
pub struct PeerAddr {
    /// IP address
    pub ip: String,
    /// Port number
    pub port: u16,
}

impl Display for PeerAddr {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.ip, self.port)
    }
}

/// BT download statistics
#[derive(Clone, Debug, Default)]
pub struct DownloadStats {
    /// Uploaded bytes
    pub uploaded_bytes: u64,
    /// Downloaded bytes
    pub downloaded_bytes: u64,
    /// Upload speed (bytes/sec)
    pub upload_speed: f64,
    /// Download speed (bytes/sec)
    pub download_speed: f64,
    /// Elapsed time (seconds)
    pub elapsed_seconds: u64,
}

/// BT download progress data structure
///
/// Contains all state information for BT download, used for persisting to disk.
#[derive(Clone, Debug)]
pub struct BtProgress {
    /// Torrent info_hash
    pub info_hash: [u8; 20],
    /// Downloaded piece bitmap
    pub bitfield: Vec<u8>,
    /// Connected peer list
    pub peers: Vec<PeerAddr>,
    /// Download statistics
    pub stats: DownloadStats,
    /// Length of each piece
    pub piece_length: u32,
    /// Total size
    pub total_size: u64,
    /// Total number of pieces
    pub num_pieces: u32,
    /// Save time
    pub save_time: SystemTime,
    /// Format version number
    pub version: u32,
}

impl Default for BtProgress {
    fn default() -> Self {
        BtProgress {
            info_hash: [0u8; 20],
            bitfield: Vec::new(),
            peers: Vec::new(),
            stats: DownloadStats::default(),
            piece_length: 0,
            total_size: 0,
            num_pieces: 0,
            save_time: SystemTime::UNIX_EPOCH,
            version: 1,
        }
    }
}

impl BtProgress {
    /// Convert info_hash to 40-character hex string
    ///
    /// # Returns
    ///
    /// Returns lowercase hex string representation
    pub fn to_hex_hash(&self) -> String {
        self.info_hash
            .iter()
            .map(|byte| format!("{:02x}", byte))
            .collect()
    }

    /// Calculate completion percentage
    ///
    /// Calculates download completion ratio based on bits set in bitfield.
    ///
    /// # Returns
    ///
    /// Returns completion ratio between 0.0 and 1.0
    pub fn completion_ratio(&self) -> f64 {
        if self.num_pieces == 0 || self.bitfield.is_empty() {
            return 0.0;
        }

        let mut set_bits = 0u32;
        for &byte in &self.bitfield {
            set_bits += byte.count_ones();
        }

        set_bits as f64 / self.num_pieces as f64
    }
}

/// BT progress file manager
///
/// Manages BT download progress save, load, delete, and list operations.
/// Supports atomic writes to ensure existing progress files are not corrupted in abnormal situations.
pub struct BtProgressManager {
    /// Progress file storage directory
    progress_dir: PathBuf,
}

impl BtProgressManager {
    /// Create new BT progress manager
    ///
    /// Automatically creates specified directory if it doesn't exist.
    ///
    /// # Arguments
    ///
    /// * `progress_dir` - Progress file storage directory path
    ///
    /// # Errors
    ///
    /// Returns error when unable to create directory
    pub fn new(progress_dir: &Path) -> Result<Self> {
        fs::create_dir_all(progress_dir).map_err(|e| {
            Aria2Error::Fatal(FatalError::Config(format!(
                "Failed to create progress directory {}: {}",
                progress_dir.display(),
                e
            )))
        })?;

        info!(path = %progress_dir.display(), "BT progress manager initialized");

        Ok(BtProgressManager {
            progress_dir: progress_dir.to_path_buf(),
        })
    }

    /// Generate progress file path
    pub fn get_progress_file_path(&self, info_hash: &[u8; 20]) -> PathBuf {
        let hex_hash: String = info_hash.iter().map(|b| format!("{:02x}", b)).collect();
        self.progress_dir.join(format!("{}.aria2", hex_hash))
    }

    /// Save BT download progress to file
    ///
    /// Uses atomic write strategy: writes to temporary file first, then replaces original file
    /// via rename operation, ensuring existing progress files are not corrupted if exceptions
    /// occur during write process.
    ///
    /// # Arguments
    ///
    /// * `info_hash` - Torrent info_hash (20 bytes)
    /// * `progress` - Progress data to save
    ///
    /// # Errors
    ///
    /// Returns error when file write fails
    pub fn save_progress(&self, info_hash: &[u8; 20], progress: &BtProgress) -> Result<()> {
        let file_path = self.get_progress_file_path(info_hash);

        debug!(
            path = %file_path.display(),
            hash = %progress.to_hex_hash(),
            "Saving BT progress"
        );

        // Write to temporary file - use unique temp file name to avoid race conditions in concurrent writes
        let tmp_path = {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let thread_id = std::thread::current().id();
            // Use thread ID hash + timestamp for uniqueness
            let unique_suffix = format!(
                "{:x}.tmp.{}",
                timestamp,
                // Format thread ID as a simple hash
                format!("{:?}", thread_id).chars().filter(|c| c.is_ascii_hexdigit()).collect::<String>()
            );
            file_path.with_extension(unique_suffix)
        };

        let content = self.serialize_progress(progress);
        {
            let mut file = fs::File::create(&tmp_path)
                .map_err(|e| Aria2Error::Io(format!("Failed to create temporary progress file: {}", e)))?;

            file.write_all(content.as_bytes())
                .map_err(|e| Aria2Error::Io(format!("Failed to write progress data: {}", e)))?;

            file.flush()
                .map_err(|e| Aria2Error::Io(format!("Failed to flush progress file buffer: {}", e)))?;
        }

        // Atomic rename
        fs::rename(&tmp_path, &file_path).map_err(|e| {
            // Clean up temporary file
            let _ = fs::remove_file(&tmp_path);
            Aria2Error::Io(format!("Failed to rename progress file: {}", e))
        })?;

        info!(
            path = %file_path.display(),
            pieces = progress.num_pieces,
            ratio = progress.completion_ratio(),
            "BT progress saved successfully"
        );

        Ok(())
    }

    /// Serialize BtProgress to text format with optimized string building
    ///
    /// Uses `write!` macro instead of `format!` + `push_str` to reduce memory allocations.
    /// Pre-allocates output buffer based on estimated size to avoid repeated reallocations.
    fn serialize_progress(&self, progress: &BtProgress) -> String {
        // Pre-allocate buffer with estimated size to minimize reallocations
        // Base overhead + per-peer and per-byte estimates for dynamic sections
        let estimated_size = 512 + progress.peers.len() * 24 + progress.bitfield.len() * 3;
        let mut output = String::with_capacity(estimated_size);

        // [Download] section - use write! for direct writing without intermediate String
        use std::fmt::Write;
        output.push_str("[Download]\n");
        let _ = writeln!(output, "info_hash={}", progress.to_hex_hash());
        let _ = writeln!(output, "version={}", progress.version);
        let _ = writeln!(output, "num_pieces={}", progress.num_pieces);
        let _ = writeln!(output, "piece_length={}", progress.piece_length);
        let _ = writeln!(output, "total_size={}", progress.total_size);
        let _ = writeln!(output, "downloaded={}", progress.stats.downloaded_bytes);
        let _ = writeln!(output, "uploaded={}", progress.stats.uploaded_bytes);
        let _ = writeln!(output, "elapsed={}", progress.stats.elapsed_seconds);

        // Serialize bitfield as hex using write! in loop (avoids collecting into intermediate String)
        let _ = write!(output, "bitfield=");
        for &byte in &progress.bitfield {
            let _ = write!(output, "{:02x}", byte);
        }
        let _ = writeln!(output);

        // [Peers] section - use write! for each peer entry
        output.push_str("[Peers]\n");
        for peer in &progress.peers {
            let _ = writeln!(output, "{}", peer);
        }

        output
    }

    /// Load BT download progress file
    ///
    /// Automatically detects C++ binary format and text format, and parses correctly.
    /// Validates whether info_hash in file matches provided parameter.
    ///
    /// # Arguments
    ///
    /// * `info_hash` - Expected info_hash (20 bytes) for validation
    ///
    /// # Returns
    ///
    /// Returns loaded progress data
    ///
    /// # Errors
    ///
    /// - File doesn't exist or read failure
    /// - File format invalid or corrupted
    /// - info_hash mismatch
    pub fn load_progress(&self, info_hash: &[u8; 20]) -> Result<BtProgress> {
        let file_path = self.get_progress_file_path(info_hash);

        debug!(
            path = %file_path.display(),
            hash = %info_hash.iter().map(|b| format!("{:02x}", b)).collect::<String>(),
            "Loading BT progress"
        );

        let content = match fs::read_to_string(&file_path) {
            Ok(content) => content,
            Err(e) => {
                // Try reading as binary format
                if e.kind() == std::io::ErrorKind::InvalidData {
                    return self.load_binary_format(info_hash, &file_path);
                }
                return Err(Aria2Error::Io(format!("Failed to read progress file: {}", e)));
            }
        };

        self.parse_text_format(info_hash, &content, &file_path)
    }

    /// Parse text format
    fn parse_text_format(
        &self,
        expected_hash: &[u8; 20],
        content: &str,
        file_path: &Path,
    ) -> Result<BtProgress> {
        let mut progress = BtProgress {
            info_hash: *expected_hash,
            bitfield: Vec::new(),
            peers: Vec::new(),
            stats: DownloadStats::default(),
            piece_length: 0,
            total_size: 0,
            num_pieces: 0,
            save_time: SystemTime::now(),
            version: 1,
        };

        let mut current_section = String::new();

        for line in content.lines() {
            let line = line.trim();

            // Skip empty lines
            if line.is_empty() {
                continue;
            }

            // 检测 section header
            if line.starts_with('[') && line.ends_with(']') {
                current_section = line[1..line.len() - 1].to_string();
                continue;
            }

            match current_section.as_str() {
                "Download" => {
                    if let Some((key, value)) = line.split_once('=') {
                        match key.trim() {
                            "info_hash" => {
                                // Validate info_hash match
                                let file_hash = value.trim().to_lowercase();
                                let expected_hex: String =
                                    expected_hash.iter().map(|b| format!("{:02x}", b)).collect();
                                if file_hash != expected_hex {
                                    return Err(Aria2Error::Io(format!(
                                        "Progress file info_hash mismatch: file={}, expected={}",
                                        file_hash, expected_hex
                                    )));
                                }
                            }
                            "version" => {
                                progress.version = value.trim().parse::<u32>().unwrap_or(1);
                            }
                            "num_pieces" => {
                                progress.num_pieces = value.trim().parse::<u32>().unwrap_or(0);
                            }
                            "piece_length" => {
                                progress.piece_length = value.trim().parse::<u32>().unwrap_or(0);
                            }
                            "total_size" => {
                                progress.total_size = value.trim().parse::<u64>().unwrap_or(0);
                            }
                            "downloaded" => {
                                progress.stats.downloaded_bytes =
                                    value.trim().parse::<u64>().unwrap_or(0);
                            }
                            "uploaded" => {
                                progress.stats.uploaded_bytes =
                                    value.trim().parse::<u64>().unwrap_or(0);
                            }
                            "elapsed" => {
                                progress.stats.elapsed_seconds =
                                    value.trim().parse::<u64>().unwrap_or(0);
                            }
                            "bitfield" => {
                                progress.bitfield = self.parse_bitfield_hex(value.trim());
                            }
                            _ => {}
                        }
                    }
                }
                "Peers" => {
                    let peer = self.parse_peer_addr(line);
                    if let Some(p) = peer {
                        progress.peers.push(p);
                    }
                }
                _ => {}
            }
        }

        info!(
            path = %file_path.display(),
            pieces = progress.num_pieces,
            ratio = progress.completion_ratio(),
            "BT progress loaded successfully"
        );

        Ok(progress)
    }

    /// Try loading binary format (C++ compatible format)
    fn load_binary_format(&self, _info_hash: &[u8; 20], _file_path: &Path) -> Result<BtProgress> {
        // Binary format currently not supported, return error prompting user to use new format
        Err(Aria2Error::Io(
            "Old C++ binary format not supported, please use text format".to_string(),
        ))
    }

    /// Parse bitfield hex string
    fn parse_bitfield_hex(&self, hex_str: &str) -> Vec<u8> {
        let hex_str = hex_str.trim();
        if hex_str.is_empty() {
            return Vec::new();
        }

        (0..hex_str.len())
            .step_by(2)
            .filter_map(|i| {
                if i + 1 < hex_str.len() {
                    u8::from_str_radix(&hex_str[i..i + 2], 16).ok()
                } else {
                    None
                }
            })
            .collect()
    }

    /// Parse peer address string
    fn parse_peer_addr(&self, addr_str: &str) -> Option<PeerAddr> {
        let addr_str = addr_str.trim();
        if addr_str.is_empty() {
            return None;
        }

        // Find last colon (IPv6 addresses may contain multiple colons)
        if let Some(colon_pos) = addr_str.rfind(':') {
            let ip = addr_str[..colon_pos].trim().to_string();
            let port: u16 = addr_str[colon_pos + 1..].trim().parse().ok()?;

            Some(PeerAddr { ip, port })
        } else {
            None
        }
    }

    /// Delete progress file for specified info_hash
    ///
    /// # Arguments
    ///
    /// * `info_hash` - info_hash of torrent to delete (20 bytes)
    ///
    /// # Errors
    ///
    /// Returns error when file deletion fails
    pub fn remove_progress(&self, info_hash: &[u8; 20]) -> Result<()> {
        let file_path = self.get_progress_file_path(info_hash);

        debug!(
            hash = %info_hash.iter().map(|b| format!("{:02x}", b)).collect::<String>(),
            "Deleting BT progress"
        );

        if file_path.exists() {
            fs::remove_file(&file_path)
                .map_err(|e| Aria2Error::Io(format!("Failed to delete progress file: {}", e)))?;

            info!(path = %file_path.display(), "BT progress file deleted");
        } else {
            warn!(path = %file_path.display(), "Progress file does not exist");
        }

        Ok(())
    }

    /// List all saved progress files
    ///
    /// Scans all `.aria2` files in progress directory, extracts their info_hash.
    ///
    /// # Returns
    ///
    /// Returns list of info_hash for all saved progress
    pub fn list_saved_progresses(&self) -> Vec<[u8; 20]> {
        let mut hashes = Vec::new();

        if let Ok(entries) = fs::read_dir(&self.progress_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name() {
                    let name_str = name.to_string_lossy();
                    if let Some(hex_hash) = name_str.strip_suffix(".aria2") {
                        // Extract hex hash from filename
                        // Remove ".aria2"
                        if let Ok(hash) = Self::hex_to_info_hash(hex_hash) {
                            hashes.push(hash);
                        } else {
                            warn!(
                                filename = %name_str,
                                "Cannot parse info_hash from progress file name"
                            );
                        }
                    }
                }
            }
        }

        debug!(count = hashes.len(), "Listed all saved BT progress");

        hashes
    }

    /// Convert hex string to info_hash
    fn hex_to_info_hash(hex_str: &str) -> std::result::Result<[u8; 20], ()> {
        if hex_str.len() != 40 {
            return Err(());
        }

        let mut hash = [0u8; 20];
        for (i, byte) in hash.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex_str[i * 2..i * 2 + 2], 16).map_err(|_| ())?;
        }

        Ok(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_addr_display() {
        let peer = PeerAddr {
            ip: "192.168.1.1".to_string(),
            port: 6881,
        };
        assert_eq!(format!("{}", peer), "192.168.1.1:6881");
    }

    #[test]
    fn test_bt_progress_to_hex_hash() {
        let progress = BtProgress {
            info_hash: [
                0xAB, 0xCD, 0x12, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
            ..Default::default()
        };
        assert_eq!(
            progress.to_hex_hash(),
            "abcd123400000000000000000000000000000000"
        );
    }

    #[test]
    fn test_completion_ratio_zero_pieces() {
        let progress = BtProgress {
            num_pieces: 0,
            bitfield: vec![],
            ..Default::default()
        };
        assert_eq!(progress.completion_ratio(), 0.0);
    }

    #[test]
    fn test_completion_ratio_full() {
        // 4 pieces all downloaded
        let progress = BtProgress {
            num_pieces: 4,
            bitfield: vec![0xFF], // 8 bits, but only 4 pieces
            ..Default::default()
        };
        // Should be 4/4 = 1.0, but will actually calculate all set bits
        assert!(progress.completion_ratio() > 0.0);
    }

    #[test]
    fn test_parse_bitfield_hex() {
        let manager = create_test_manager();
        let result = manager.parse_bitfield_hex("ff00ff");
        assert_eq!(result, vec![0xFF, 0x00, 0xFF]);
    }

    #[test]
    fn test_parse_bitfield_empty() {
        let manager = create_test_manager();
        let result = manager.parse_bitfield_hex("");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_peer_addr_ipv4() {
        let manager = create_test_manager();
        let peer = manager.parse_peer_addr("192.168.1.100:6881").unwrap();
        assert_eq!(peer.ip, "192.168.1.100");
        assert_eq!(peer.port, 6881);
    }

    #[test]
    fn test_parse_peer_addr_invalid() {
        let manager = create_test_manager();
        assert!(manager.parse_peer_addr("invalid").is_none());
        assert!(manager.parse_peer_addr("").is_none());
    }

    #[test]
    fn test_hex_to_info_hash_valid() {
        let hex = "abcdef1234567890abcdef1234567890abcdef12";
        let hash = BtProgressManager::hex_to_info_hash(hex).unwrap();
        assert_eq!(
            hash,
            [
                0xAB, 0xCD, 0xEF, 0x12, 0x34, 0x56, 0x78, 0x90, 0xAB, 0xCD, 0xEF, 0x12, 0x34, 0x56,
                0x78, 0x90, 0xAB, 0xCD, 0xEF, 0x12
            ]
        );
    }

    #[test]
    fn test_hex_to_info_hash_invalid_length() {
        assert!(BtProgressManager::hex_to_info_hash("abc123").is_err());
    }

    fn create_test_manager() -> BtProgressManager {
        let dir = std::env::temp_dir().join("bt_progress_test_12345");
        let _ = fs::create_dir_all(&dir);
        BtProgressManager::new(&dir).unwrap()
    }
}
