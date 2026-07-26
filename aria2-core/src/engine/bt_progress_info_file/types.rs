//! Data types for BitTorrent progress persistence.
//!
//! Contains the core data structures that represent download progress,
//! statistics, in-flight pieces, and peer addresses. These types are
//! shared between the binary and text format (de)serializers.

use std::time::SystemTime;

// ===========================================================================
// PeerAddr — persisted peer address for resume
// ===========================================================================

/// A peer address persisted in the progress file for resume reconnection.
///
/// Mirrors C++ peer entries saved in `DefaultBtProgressInfoFile::save()`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerAddr {
    /// IP address string (e.g., "192.168.1.100")
    pub ip: String,
    /// Port number
    pub port: u16,
}

// ===========================================================================
// DownloadStats — cumulative transfer statistics
// ===========================================================================

/// Cumulative upload/download statistics for a torrent.
///
/// Mirrors fields from C++ `DownloadContext` and `BtRuntime` that are
/// persisted in the progress file. Note: these stats are only stored
/// in the text format; the binary format does not include them.
#[derive(Debug, Clone, PartialEq)]
pub struct DownloadStats {
    /// Total bytes uploaded
    pub uploaded_bytes: u64,
    /// Total bytes downloaded
    pub downloaded_bytes: u64,
    /// Current upload speed in bytes/sec
    pub upload_speed: f64,
    /// Current download speed in bytes/sec
    pub download_speed: f64,
    /// Total elapsed seconds since download started
    pub elapsed_seconds: u64,
}

impl Default for DownloadStats {
    fn default() -> Self {
        Self {
            uploaded_bytes: 0,
            downloaded_bytes: 0,
            upload_speed: 0.0,
            download_speed: 0.0,
            elapsed_seconds: 0,
        }
    }
}

// ===========================================================================
// InFlightPiece — partially-downloaded piece with block-level bitfield
// ===========================================================================

/// A partially-downloaded piece with a block-level completion bitfield.
///
/// When a download is interrupted, some pieces may have some blocks
/// downloaded but not all. This struct tracks which blocks within
/// the piece are complete so that only missing blocks are re-requested
/// on resume.
///
/// Mirrors C++ in-flight piece tracking in `DefaultBtProgressInfoFile`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlightPiece {
    /// Piece index
    pub index: u32,
    /// Piece length in bytes
    pub length: u32,
    /// Block-level completion bitfield (1 bit per block)
    pub bitfield: Vec<u8>,
}

impl InFlightPiece {
    /// Create a new in-flight piece with the given block bitfield.
    pub fn new(index: u32, length: u32, bitfield: Vec<u8>) -> Self {
        Self {
            index,
            length,
            bitfield,
        }
    }

    /// Compute the number of completed blocks.
    pub fn completed_blocks(&self) -> u32 {
        self.bitfield
            .iter()
            .map(|b| b.count_ones())
            .sum()
    }

    /// Check if all blocks are complete.
    pub fn is_complete(&self, num_blocks: u32) -> bool {
        self.completed_blocks() >= num_blocks
    }
}

// ===========================================================================
// BtProgress — snapshot of download progress
// ===========================================================================

/// Snapshot of BitTorrent download progress for persistence.
///
/// Contains all information needed to resume a download after restart:
/// - Piece completion bitfield
/// - In-flight pieces with block-level completion
/// - Peer list for reconnection
/// - Cumulative transfer statistics
///
/// Mirrors the state persisted by C++ `DefaultBtProgressInfoFile::save()`.
#[derive(Debug, Clone)]
pub struct BtProgress {
    /// 20-byte info hash of the torrent
    pub info_hash: [u8; 20],
    /// Piece completion bitfield (1 bit per piece)
    pub bitfield: Vec<u8>,
    /// Saved peer addresses for resume reconnection
    pub peers: Vec<PeerAddr>,
    /// Cumulative transfer statistics
    pub stats: DownloadStats,
    /// Piece length in bytes
    pub piece_length: u32,
    /// Total size of the torrent in bytes
    pub total_size: u64,
    /// Number of pieces in the torrent
    pub num_pieces: u32,
    /// Total bytes uploaded (cumulative across sessions)
    pub upload_length: u64,
    /// In-flight pieces with partial block completion
    pub in_flight_pieces: Vec<InFlightPiece>,
    /// Whether this is a torrent download (vs. HTTP/FTP)
    pub is_torrent: bool,
    /// Time when this progress was saved
    pub save_time: SystemTime,
    /// Progress file format version
    pub version: u32,
}

impl Default for BtProgress {
    fn default() -> Self {
        Self {
            info_hash: [0u8; 20],
            bitfield: Vec::new(),
            peers: Vec::new(),
            stats: DownloadStats::default(),
            piece_length: 0,
            total_size: 0,
            num_pieces: 0,
            upload_length: 0,
            in_flight_pieces: Vec::new(),
            is_torrent: false,
            save_time: SystemTime::now(),
            version: 1,
        }
    }
}

impl BtProgress {
    /// Compute the completion ratio (0.0 to 1.0) based on the bitfield.
    ///
    /// Counts set bits in the bitfield up to `num_pieces` and divides
    /// by `num_pieces`. Returns 0.0 if `num_pieces` is 0.
    pub fn completion_ratio(&self) -> f64 {
        if self.num_pieces == 0 || self.bitfield.is_empty() {
            return 0.0;
        }
        let set_bits: u32 = self
            .bitfield
            .iter()
            .map(|b| b.count_ones())
            .sum();
        let effective_bits = set_bits.min(self.num_pieces);
        effective_bits as f64 / self.num_pieces as f64
    }

    /// Count the number of completed pieces (set bits in bitfield).
    pub fn num_completed_pieces(&self) -> u32 {
        self.bitfield
            .iter()
            .map(|b| b.count_ones())
            .sum::<u32>()
            .min(self.num_pieces)
    }

    /// Convert the 20-byte info_hash to a lowercase hex string.
    ///
    /// Matches C++ `BtProgressInfoFile::toHexHash()` which returns
    /// the SHA-1 info hash as a 40-character lowercase hex string.
    pub fn to_hex_hash(&self) -> String {
        self.info_hash.iter().map(|b| format!("{:02x}", b)).collect()
    }
}

// ===========================================================================
// Helper functions
// ===========================================================================

/// Parse a 40-character hex string into a 20-byte info hash.
pub fn hex_to_info_hash(hex: &str) -> std::result::Result<[u8; 20], ()> {
    if hex.len() != 40 {
        return Err(());
    }
    let mut hash = [0u8; 20];
    for i in 0..20 {
        hash[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| ())?;
    }
    Ok(hash)
}

/// Convert a 20-byte info hash to a lowercase hex string.
pub fn info_hash_to_hex(info_hash: &[u8; 20]) -> String {
    info_hash.iter().map(|b| format!("{:02x}", b)).collect()
}
