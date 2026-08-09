//! BtRegistry — Global registry for BitTorrent-related components.
//!
//! Maps GID (download ID) to [`BtObject`], which bundles all shared state
//! for a single BitTorrent download: `DownloadContext`, `PieceStorage`,
//! `PeerStorage`, `BtAnnounce`, and `BtProgressManager`.
//!
//! # Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/BtRegistry.h` / `src/BtRegistry.cc` — Registry + BtObject
//!
//! # Design Differences from C++ aria2
//!
//! | C++ aria2 | Rust | Rationale |
//! |---|---|---|
//! | `unique_ptr<BtObject>` in pool | `BtObject` owned directly in `HashMap` | No heap indirection; Rust ownership suffices |
//! | `shared_ptr<DownloadContext>` | `Arc<DownloadContext>` | Same shared-ownership semantics |
//! | `shared_ptr<PieceStorage>` | `Option<Arc<dyn PieceStorage>>` | Same shared-ownership semantics via trait object |
//! | `shared_ptr<PeerStorage>` | `Option<Arc<dyn PeerStorage>>` | Same shared-ownership semantics via trait object |
//! | `shared_ptr<BtAnnounce>` | `Option<Arc<BtAnnounce>>` | Same shared-ownership semantics |
//! | `shared_ptr<BtProgressInfoFile>` | `Option<Arc<BtProgressManager>>` | Rust equivalent with modern async API |
//! | `shared_ptr<LpdMessageReceiver>` | `Option<u64>` ID-based reference | Type not yet implemented |
//! | `shared_ptr<UDPTrackerClient>` | `Option<u64>` ID-based reference | Type not yet implemented |
//! | `shared_ptr<DHT::DhtNodeLookup>` | `Option<Arc<DhtEngine>>` | Direct Arc reference; DhtEngine is already shared |
//! | `getNull<T>()` for missing entries | `Option<T>` | Rust-idiomatic null handling |
//! | `OutputIterator` for getAllDownloadContext | `Vec<Arc<DownloadContext>>` | Simpler, Rust-idiomatic API |
//! | Linear scan for info_hash lookup | `HashMap<String, u64>` secondary index | O(1) instead of O(n) |

mod operations;
mod types;

#[cfg(test)]
mod tests;

pub use types::{BtObject, BtObjectBuilder};

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use crate::engine::bt_peer_blocklist::BtPeerBlocklist;

// ===========================================================================
// BtRegistry
// ===========================================================================

/// Global registry for BitTorrent-related components.
///
/// Maps GID (download ID) to [`BtObject`]. Also holds global BT settings
/// like TCP/UDP listen ports, the shared DHT engine, and references to
/// singleton services (LPD message receiver, UDP tracker client).
///
/// # Thread Safety
///
/// `BtRegistry` is designed to be used behind an external synchronization
/// primitive (e.g., `Mutex<BtRegistry>` or `RwLock<BtRegistry>`) when
/// shared across threads. This matches the C++ pattern where `BtRegistry`
/// is accessed through a locked `DownloadEngine`.
///
/// # C++ Reference
///
/// Equivalent to `BtRegistry` class in `BtRegistry.h` / `BtRegistry.cc`.
pub struct BtRegistry {
    /// GID -> BtObject mapping. In C++ aria2, this uses
    /// `std::map<a2_gid_t, std::unique_ptr<BtObject>>`. Here we own
    /// BtObject directly in the HashMap value, avoiding heap indirection.
    pub(crate) pool: HashMap<u64, BtObject>,

    /// Secondary index: info_hash hex string -> GID for O(1) lookup.
    /// C++ performs a linear scan over all entries; this index avoids that.
    pub(crate) info_hash_index: HashMap<String, u64>,

    /// Shared DHT engine for all torrents in this session.
    /// In C++ aria2, the DHT node is a process-level singleton accessed
    /// via `DHT::getInstance()`. Here we store it as an `Arc<DhtEngine>`
    /// owned by the registry.
    dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,

    /// TCP listen port for incoming BitTorrent connections.
    tcp_port: u16,

    /// UDP port for DHT and UDP tracker. Note: UDP tracker is not
    /// supported in IPv6 (same limitation as C++ aria2).
    udp_port: u16,

    /// ID-based reference to the LPD message receiver.
    /// LpdMessageReceiver is not yet implemented as a type that can
    /// be stored here directly; use an ID to look it up in a global registry.
    lpd_message_receiver_id: Option<u64>,

    /// ID-based reference to the UDP tracker client.
    /// UDPTrackerClient is not yet implemented as a type that can
    /// be stored here directly; use an ID to look it up in a global registry.
    udp_tracker_client_id: Option<u64>,

    /// IP range-based blocklist for rejecting peers by address.
    /// In C++ aria2, this is `shared_ptr<BtPeerBlocklist> peerBlocklist_`.
    peer_blocklist: BtPeerBlocklist,
}

impl BtRegistry {
    /// Create a new `BtRegistry` with default values.
    ///
    /// - `tcp_port` = 0 (not assigned)
    /// - `udp_port` = 0 (not assigned)
    /// - Empty pool, no DHT engine, no LPD receiver, no UDP tracker client.
    pub fn new() -> Self {
        Self {
            pool: HashMap::new(),
            info_hash_index: HashMap::new(),
            dht_engine: None,
            tcp_port: 0,
            udp_port: 0,
            lpd_message_receiver_id: None,
            udp_tracker_client_id: None,
            peer_blocklist: BtPeerBlocklist::new(),
        }
    }
}

impl Default for BtRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for BtRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BtRegistry")
            .field("pool_len", &self.pool.len())
            .field("info_hash_index_len", &self.info_hash_index.len())
            .field("has_dht_engine", &self.dht_engine.is_some())
            .field("tcp_port", &self.tcp_port)
            .field("udp_port", &self.udp_port)
            .field("lpd_message_receiver_id", &self.lpd_message_receiver_id)
            .field("udp_tracker_client_id", &self.udp_tracker_client_id)
            .field("blocklist_count", &self.peer_blocklist.count())
            .finish()
    }
}
