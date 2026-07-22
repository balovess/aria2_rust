//! DHT state snapshot types for session persistence.

use serde::{Deserialize, Serialize};

/// Snapshot of DHT (Distributed Hash Table) routing state for persistence.
///
/// Captures the current state of DHT nodes, token secret, and bootstrap timing
/// to allow quick resumption without full bootstrap on restart.
///
/// # Serialization
///
/// This struct implements Serialize/Deserialize for JSON persistence alongside
/// session data. Use `to_json_string()` and `from_json_string()` for conversion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DhtStateSnapshot {
    /// Known DHT nodes in the routing table
    pub nodes: Vec<DhtNodeInfo>,
    /// Current token secret used for DHT get_peers requests (20 bytes)
    pub token_secret: [u8; 20],
    /// Unix epoch timestamp of last successful bootstrap, if any
    pub last_bootstrap_epoch_secs: Option<u64>,
    /// Total number of nodes in the snapshot (convenience field)
    pub total_nodes: usize,
}

/// Information about a single DHT node in the routing table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DhtNodeInfo {
    /// 20-byte node ID (SHA-1 hash)
    pub id: [u8; 20],
    /// Network address as "ip:port" string
    pub addr: String,
    /// Unix epoch timestamp when this node was last seen/verified
    pub last_seen_epoch_secs: u64,
}

impl DhtStateSnapshot {
    /// Create an empty snapshot (for when DHT is unavailable or not initialized).
    ///
    /// Returns a snapshot with no nodes and zeroed token secret,
    /// suitable as a default or placeholder value.
    pub fn empty() -> Self {
        Self {
            nodes: vec![],
            token_secret: [0u8; 20],
            last_bootstrap_epoch_secs: None,
            total_nodes: 0,
        }
    }

    /// Create a snapshot from node data with automatic total_nodes calculation.
    ///
    /// # Arguments
    ///
    /// * `nodes` - Vector of known DHT nodes
    /// * `token_secret` - Current 20-byte token secret
    /// * `last_bootstrap` - Optional timestamp of last bootstrap
    pub fn new(
        nodes: Vec<DhtNodeInfo>,
        token_secret: [u8; 20],
        last_bootstrap_epoch_secs: Option<u64>,
    ) -> Self {
        let total_nodes = nodes.len();
        Self {
            nodes,
            token_secret,
            last_bootstrap_epoch_secs,
            total_nodes,
        }
    }

    /// Serialize snapshot to JSON string for persistence.
    ///
    /// # Returns
    ///
    /// * `Ok(String)` - JSON-formatted snapshot data
    /// * `Err(String)` - Error message if serialization fails
    pub fn to_json_string(&self) -> Result<String, String> {
        serde_json::to_string(self).map_err(|e| e.to_string())
    }

    /// Parse snapshot from JSON string.
    ///
    /// # Arguments
    ///
    /// * `json` - JSON string containing serialized snapshot data
    ///
    /// # Returns
    ///
    /// * `Ok(DhtStateSnapshot)` - Deserialized snapshot
    /// * `Err(String)` - Error message if parsing fails
    pub fn from_json_string(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
    }
}
