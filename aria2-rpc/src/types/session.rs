//! Global statistics, version, session, and GID data.

use serde::{Deserialize, Serialize};

use crate::wire;

// =========================================================================
// Global Statistics
// =========================================================================

/// Global download statistics.
///
/// Returned by `aria2.getGlobalStat`. Contains aggregate numbers for
/// active, waiting, and stopped downloads, plus total transfer speeds.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GlobalStat {
    #[serde(
        serialize_with = "wire::serialize_display_as_string",
        deserialize_with = "wire::deserialize_string_or_number"
    )]
    pub download_speed: u64,
    #[serde(
        serialize_with = "wire::serialize_display_as_string",
        deserialize_with = "wire::deserialize_string_or_number"
    )]
    pub upload_speed: u64,
    #[serde(
        serialize_with = "wire::serialize_display_as_string",
        deserialize_with = "wire::deserialize_string_or_number"
    )]
    pub num_active: usize,
    #[serde(
        serialize_with = "wire::serialize_display_as_string",
        deserialize_with = "wire::deserialize_string_or_number"
    )]
    pub num_waiting: usize,
    #[serde(
        serialize_with = "wire::serialize_display_as_string",
        deserialize_with = "wire::deserialize_string_or_number"
    )]
    pub num_stopped: usize,
    #[serde(
        serialize_with = "wire::serialize_display_as_string",
        deserialize_with = "wire::deserialize_string_or_number"
    )]
    pub num_stopped_total: usize,
}

/// Process-wide DHT runtime counters returned by `aria2.getDhtStatus`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DhtStatus {
    pub state: String,
    #[serde(
        serialize_with = "wire::serialize_display_as_string",
        deserialize_with = "wire::deserialize_string_or_number"
    )]
    pub total_nodes: usize,
    #[serde(
        serialize_with = "wire::serialize_display_as_string",
        deserialize_with = "wire::deserialize_string_or_number"
    )]
    pub good_nodes: usize,
    #[serde(
        serialize_with = "wire::serialize_display_as_string",
        deserialize_with = "wire::deserialize_string_or_number"
    )]
    pub pending_transactions: usize,
}

impl GlobalStat {
    /// Serialize as JSON matching original aria2 wire format where all
    /// numeric values are strings (e.g. `"downloadSpeed": "0"`, `"numActive": "1"`).
    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("GlobalStat contains only serializable fields")
    }
}

// =========================================================================
// Version and Session Types
// =========================================================================

/// Version information returned by `aria2.getVersion`.
///
/// Contains this product's version and the aria2-compatible feature list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionInfo {
    /// Product version string.
    pub version: String,
    /// List of enabled feature names (serialized as "enabledFeatures" in JSON)
    #[serde(rename = "enabledFeatures")]
    pub enabled_features: Vec<String>,
}

impl VersionInfo {
    /// Create our product version in the aria2-compatible public shape.
    ///
    /// Enabled features are dynamically generated based on compile-time
    /// protocol support available in the current RPC build. The embedding
    /// application can provide its own product version and feature catalog.
    pub fn from_env() -> Self {
        Self::from_version(env!("CARGO_PKG_VERSION"))
    }

    /// Create version information for an embedding product.
    ///
    /// Library callers default to the `aria2-rpc` package version through
    /// [`Self::from_env`]. The `aria2` binary passes its own release version
    /// here so RPC `getVersion` reports the binary product that is running.
    pub fn from_version(version: impl Into<String>) -> Self {
        // Keep the order and names used by C++ FeatureConfig::strSupportedFeature().
        let mut features = vec!["Async DNS"];
        #[cfg(feature = "bittorrent")]
        features.push("BitTorrent");
        features.extend(["Firefox3 Cookie", "GZip", "HTTPS", "Message Digest"]);
        #[cfg(feature = "metalink")]
        features.push("Metalink");
        features.push("XML-RPC");
        #[cfg(feature = "sftp")]
        features.push("SFTP");

        Self {
            version: version.into(),
            enabled_features: features.into_iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Convert to JSON-RPC response value (camelCase keys).
    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "enabledFeatures": self.enabled_features,
            "version": self.version
        })
    }
}

/// Session information returned by `aria2.getSessionInfo`.
///
/// Contains session identifier and startup timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    /// Unique session identifier
    pub session_id: String,
    /// Session start time as Unix timestamp (seconds since epoch)
    pub session_start_time: u64,
}

impl SessionInfo {
    /// Create a new SessionInfo with an aria2-compatible session identifier.
    ///
    /// The original DownloadEngine generates 20 random bytes at construction
    /// time and exposes their lowercase hexadecimal representation through
    /// `getSessionInfo`.
    pub fn new() -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let start_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            session_id: generate_session_id(),
            session_start_time: start_time,
        }
    }

    /// Convert to JSON-RPC response value (camelCase key).
    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "sessionId": self.session_id
        })
    }
}

/// Generate the session identifier exposed by `aria2.getSessionInfo`.
///
/// This matches the original aria2 wire shape: 20 random bytes encoded as 40
/// lowercase hexadecimal characters. The identifier is generated once by
/// [`SessionInfo::new`] when an RPC engine is constructed.
pub fn generate_session_id() -> String {
    use rand::RngCore;

    const SESSION_ID_BYTES: usize = 20;
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut bytes = [0u8; SESSION_ID_BYTES];
    rand::thread_rng().fill_bytes(&mut bytes);

    let mut session_id = String::with_capacity(SESSION_ID_BYTES * 2);
    for byte in bytes {
        session_id.push(HEX[(byte >> 4) as usize] as char);
        session_id.push(HEX[(byte & 0x0f) as usize] as char);
    }
    session_id
}

impl Default for SessionInfo {
    fn default() -> Self {
        Self::new()
    }
}

// =========================================================================
// GID Generation
// =========================================================================

fn generate_gid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    nanos.hash(&mut hasher);
    rand::random::<u64>().hash(&mut hasher);
    format!("{:01$x}", hasher.finish(), crate::constants::GID_HEX_DIGITS)
}

/// Generate a unique GID (Global IDentifier) for a download task.
///
/// Uses a combination of current time (nanoseconds), a hash, and a random
/// value to produce a 16-character hexadecimal identifier.
pub fn create_gid() -> String {
    generate_gid()
}
