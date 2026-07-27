//! Type definitions for download context attributes: ContextAttributeType,
//! BtFileMode, TorrentAttribute, and Signature.

// ---------------------------------------------------------------------------
// ContextAttributeType
// ---------------------------------------------------------------------------

/// Typed keys for the attribute extension map on `DownloadContext`.
///
/// Mirrors the C++ `ContextAttributeType` enum. The `Ed2k` variant is an
/// aria2-next addition; `BitTorrent` is the original attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContextAttributeType {
    BitTorrent,
    Ed2k,
}

// ---------------------------------------------------------------------------
// TorrentAttribute — BitTorrent-specific download metadata
// ---------------------------------------------------------------------------

/// BitTorrent file mode — single vs multi-file torrent.
///
/// Mirrors C++ `BtFileMode` enum. Used in `TorrentAttribute::mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
pub enum BtFileMode {
    /// Single-file torrent (one file in the info dict).
    #[default]
    Single,
    /// Multi-file torrent (directory with multiple files in the info dict).
    Multi,
}


/// BitTorrent-specific attributes stored on `DownloadContext`.
///
/// Mirrors C++ `bittorrent::TorrentAttribute` which is accessed via
/// `bittorrent::getTorrentAttrs(DownloadContext*)`. In C++ this is a struct
/// inheriting from `ContextAttribute` with the following fields:
/// `name`, `mode`, `announceList`, `nodes`, `infoHash`, `metadata`,
/// `metadataSize`, `privateTorrent`, `creationDate`, `comment`,
/// `createdBy`, `urlList`.
///
/// All fields from C++ are present here. The Rust version uses owned types
/// instead of C++ raw pointers/strings.
#[derive(Debug, Clone)]
pub struct TorrentAttribute {
    /// Torrent name from the info dict.
    /// C++ `name` — e.g. "debian-13.5.0-amd64-DVD-1"
    pub name: String,

    /// File mode (single vs multi).
    /// C++ `mode` — `BtFileMode::SINGLE` or `BtFileMode::MULTI`
    pub mode: BtFileMode,

    /// Announce URL list from the .torrent file or magnet URI.
    /// C++ `announceList` — tiered list of tracker URLs.
    pub announce_list: Vec<Vec<String>>,

    /// DHT bootstrap nodes from the .torrent file.
    /// C++ `nodes` — `vector<pair<string, uint16_t>>` for DHT bootstrap.
    pub nodes: Vec<(String, u16)>,

    /// 20-byte info hash in hexadecimal (40 chars).
    /// C++ `infoHash` — identifies the torrent for tracker/DHT/PEX.
    pub info_hash: String,

    /// Raw torrent metadata (bencoded info dict bytes).
    /// C++ `metadata` — used for ut_metadata extension (BEP 9).
    /// Empty for regular torrents (metadata already available), populated
    /// for magnet links after metadata exchange completes.
    pub metadata: Vec<u8>,

    /// Size of the metadata in bytes (for ut_metadata extension).
    /// C++ `metadataSize` — 0 when metadata is already available.
    pub metadata_size: usize,

    /// Whether this is a private torrent (BEP 0027).
    /// C++ `privateTorrent` — when true, DHT/PEX/LPD must be disabled.
    pub private_torrent: bool,

    /// Creation date from the .torrent file (Unix timestamp).
    /// C++ `creationDate` — 0 when not present in the torrent.
    pub creation_date: i64,

    /// Comment from the .torrent file.
    /// C++ `comment` — empty when not present.
    pub comment: String,

    /// Creator field from the .torrent file.
    /// C++ `createdBy` — empty when not present.
    pub created_by: String,

    /// Web seed URLs from the .torrent url-list field.
    /// C++ `urlList` — HTTP/FTP seeds for hybrid downloading.
    pub url_list: Vec<String>,
}

impl TorrentAttribute {
    /// Create a new `TorrentAttribute` with the given info hash.
    ///
    /// All other fields default to empty/zero values. This is the minimal
    /// constructor used when only the info hash is known (e.g. magnet link
    /// before metadata exchange).
    pub fn new(info_hash: String) -> Self {
        Self {
            name: String::new(),
            mode: BtFileMode::Single,
            announce_list: Vec::new(),
            nodes: Vec::new(),
            info_hash,
            metadata: Vec::new(),
            metadata_size: 0,
            private_torrent: false,
            creation_date: 0,
            comment: String::new(),
            created_by: String::new(),
            url_list: Vec::new(),
        }
    }

    /// Create a `TorrentAttribute` from a 20-byte raw info hash.
    pub fn from_bytes(info_hash_bytes: &[u8; 20]) -> Self {
        Self::new(hex::encode(info_hash_bytes))
    }

    /// Whether the metadata has been received (for magnet links).
    ///
    /// In C++, this is checked via `metadata.size() > 0`. We use
    /// `metadata_size > 0 || !metadata.is_empty()` which is equivalent.
    pub fn metadata_received(&self) -> bool {
        self.metadata_size > 0 || !self.metadata.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Signature
// ---------------------------------------------------------------------------

/// Placeholder for Metalink / PGP signature data.
///
/// Will be expanded with actual PGP parsing when Metalink support is
/// fully wired in.
#[derive(Debug, Clone)]
pub struct Signature {
    /// Raw signature body (ASCII-armored or binary)
    pub body: String,
    /// Hash algorithm used for the signature (e.g. "sha-1", "sha-256")
    pub hash_type: String,
}

impl Signature {
    /// Create a new signature with the given body and hash type.
    pub fn new(body: String, hash_type: String) -> Self {
        Self { body, hash_type }
    }
}
