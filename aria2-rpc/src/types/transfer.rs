//! URI, server, and BitTorrent peer data returned by RPC methods.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::wire;

/// URI entry with status tracking.
///
/// Used in `FileInfo.uris` and returned by `aria2.getUris`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UriEntry {
    pub uri: String,
    pub status: UriStatus,
}

impl UriEntry {
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            status: UriStatus::Waiting,
        }
    }
    pub fn used(mut self) -> Self {
        self.status = UriStatus::Used;
        self
    }
    pub fn waiting(mut self) -> Self {
        self.status = UriStatus::Waiting;
        self
    }
}

/// URI status indicating whether a URI is currently being used or waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UriStatus {
    Used,
    Spent,
    #[default]
    Waiting,
}

impl Serialize for UriStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Used | Self::Spent => "used",
            Self::Waiting => "waiting",
        })
    }
}

impl<'de> Deserialize<'de> for UriStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "used" => Ok(Self::Used),
            "waiting" => Ok(Self::Waiting),
            // Accepted for in-process snapshots; never emitted on the wire.
            "spent" => Ok(Self::Spent),
            _ => Err(serde::de::Error::unknown_variant(
                &value,
                &["used", "waiting"],
            )),
        }
    }
}

/// URI information returned by `aria2.getUris`.
///
/// Type alias for [`UriEntry`] for API compatibility.
pub type UriInfo = UriEntry;

// =========================================================================
// Server and Peer Types
// =========================================================================

/// Server connection information for a specific file index.
///
/// Returned by `aria2.getServers`, grouped by file index.
/// All numeric fields are serialized as strings matching original aria2c.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfoIndex {
    /// File index (1-based, serialized as string matching original aria2c)
    #[serde(
        serialize_with = "wire::serialize_display_as_string",
        deserialize_with = "wire::deserialize_string_or_number"
    )]
    pub index: usize,
    /// List of active server connections for this file
    pub servers: Vec<ServerInfo>,
}

/// Individual server connection details.
///
/// Contains URI, current active URI (after redirects), and download speed.
/// All numeric fields are serialized as strings matching original aria2c.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    /// Original server URI
    pub uri: String,
    /// Current active URI (may differ from original after redirects)
    pub current_uri: String,
    /// Current download speed from this server (bytes/sec, serialized as string)
    #[serde(
        serialize_with = "wire::serialize_display_as_string",
        deserialize_with = "wire::deserialize_string_or_number"
    )]
    pub download_speed: u64,
}

impl ServerInfo {
    /// Create a new ServerInfo instance.
    pub fn new(uri: impl Into<String>) -> Self {
        let uri_str = uri.into();
        Self {
            current_uri: uri_str.clone(),
            uri: uri_str,
            download_speed: 0,
        }
    }

    /// Set the current (possibly redirected) URI.
    pub fn with_current_uri(mut self, uri: impl Into<String>) -> Self {
        self.current_uri = uri.into();
        self
    }

    /// Set the download speed.
    pub fn with_download_speed(mut self, speed: u64) -> Self {
        self.download_speed = speed;
        self
    }
}

/// BitTorrent peer information.
///
/// Returned by `aria2.getPeers`. Contains peer connection state and
/// transfer speeds. Matches original aria2 peer entry fields.
/// All numeric fields and boolean fields are serialized as strings
/// matching original aria2c wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerInfo {
    pub peer_id: String,
    pub ip: String,
    /// Peer port (serialized as string matching original util::uitos)
    #[serde(
        serialize_with = "wire::serialize_display_as_string",
        deserialize_with = "wire::deserialize_string_or_number"
    )]
    pub port: u16,
    /// Bitfield hex string (matches original util::toHex)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitfield: Option<String>,
    /// Whether we are choking this peer (serialized as "true"/"false")
    #[serde(
        serialize_with = "wire::serialize_bool_as_string",
        deserialize_with = "wire::deserialize_bool_from_string_or_bool"
    )]
    pub am_choking: bool,
    /// Whether the peer is choking us (serialized as "true"/"false")
    #[serde(
        serialize_with = "wire::serialize_bool_as_string",
        deserialize_with = "wire::deserialize_bool_from_string_or_bool"
    )]
    pub peer_choking: bool,
    /// Download speed (serialized as string matching original util::itos)
    #[serde(
        serialize_with = "wire::serialize_display_as_string",
        deserialize_with = "wire::deserialize_string_or_number"
    )]
    pub download_speed: u64,
    /// Upload speed (serialized as string matching original util::itos)
    #[serde(
        serialize_with = "wire::serialize_display_as_string",
        deserialize_with = "wire::deserialize_string_or_number"
    )]
    pub upload_speed: u64,
    /// Seeder status as "true"/"false" string (matches original VLB_TRUE/VLB_FALSE)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seeder: Option<String>,
}
