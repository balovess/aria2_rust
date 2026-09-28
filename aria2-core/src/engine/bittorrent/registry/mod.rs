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
//! | `shared_ptr<DHT::DhtNodeLookup>` | `DhtEngineSet` | One process engine per IP family, shared by Arc |
//! | `getNull<T>()` for missing entries | `Option<T>` | Rust-idiomatic null handling |
//! | `OutputIterator` for getAllDownloadContext | `Vec<Arc<DownloadContext>>` | Simpler, Rust-idiomatic API |
//! | Linear scan for info_hash lookup | `HashMap<String, u64>` secondary index | O(1) instead of O(n) |

mod operations;
mod types;

#[cfg(test)]
mod tests;

pub use types::{BtObject, BtObjectBuilder};

use crate::engine::bittorrent::dht::engine_set::DhtEngineSet;
use crate::engine::bittorrent::peer::blocklist::BtPeerBlocklist;
use std::collections::HashMap;
use std::fmt;

// ===========================================================================
// BtRegistry
// ===========================================================================

/// Global registry for BitTorrent-related components.
///
/// Maps GID (download ID) to [`BtObject`]. Also holds global BT settings
/// like TCP listen ports and one shared DHT engine for each enabled IP family.
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

    /// One process-wide DHT engine per address family, matching aria2's
    /// independent IPv4 and IPv6 DHT registries.
    global_dht_engines: DhtEngineSet,

    /// Serialize first-use startup independently for IPv4 and IPv6. The
    /// registry's outer lock must not be held across asynchronous socket or
    /// persistence I/O.
    dht_engine_start_locks: [std::sync::Arc<tokio::sync::Mutex<()>>; 2],

    /// Download handles referencing the shared DHT engine, keyed by GID.
    ///
    /// Keeping these references lets lifecycle cleanup remove a task without
    /// stopping the process-wide engines used by the remaining BT session.
    dht_engines: HashMap<u64, DhtEngineSet>,

    /// TCP listen port for incoming BitTorrent connections.
    tcp_port: u16,

    /// UDP port for DHT and UDP tracker. Note: UDP tracker is not
    /// supported in IPv6 (same limitation as C++ aria2).
    udp_port: u16,

    /// IP range-based blocklist for rejecting peers by address.
    /// In C++ aria2, this is `shared_ptr<BtPeerBlocklist> peerBlocklist_`.
    peer_blocklist: BtPeerBlocklist,
}

impl BtRegistry {
    /// Create a new `BtRegistry` with default values.
    ///
    /// - `tcp_port` = 0 (not assigned)
    /// - `udp_port` = 0 (not assigned)
    /// - Empty pool and no DHT engine.
    pub fn new() -> Self {
        Self {
            pool: HashMap::new(),
            info_hash_index: HashMap::new(),
            global_dht_engines: DhtEngineSet::default(),
            dht_engine_start_locks: [
                std::sync::Arc::new(tokio::sync::Mutex::new(())),
                std::sync::Arc::new(tokio::sync::Mutex::new(())),
            ],
            dht_engines: HashMap::new(),
            tcp_port: 0,
            udp_port: 0,
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
            .field("dht_engine_count", &self.global_dht_engines.iter().count())
            .field("tcp_port", &self.tcp_port)
            .field("udp_port", &self.udp_port)
            .field("blocklist_count", &self.peer_blocklist.count())
            .finish()
    }
}
