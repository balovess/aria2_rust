//! DefaultPeerStorage — Peer lifecycle management for BitTorrent.
//!
//! Manages the complete peer lifecycle: unused (discovered) peers → used
//! (connected) peers → dropped (recently disconnected) peers.
//!
//! # C++ Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/DefaultPeerStorage.h` / `src/DefaultPeerStorage.cc`
//!
//! # Key Data Structures
//!
//! | C++ aria2 | Rust | Rationale |
//! |---|---|---|
//! | `set<pair<string, uint16_t>>` | `HashSet<(String, u16)>` | Same dedup by (ip, port) |
//! | `deque<shared_ptr<Peer>>` | `VecDeque<PeerEntry>` | Same FIFO ordering |
//! | `PeerSet` (sorted by ptr) | `HashSet<PeerEntry>` | Identity by (ip, port) suffices |
//! | `map<string, Timer>` | `HashMap<String, Instant>` | Same ip → timeout mapping |
//! | `unique_ptr<BtSeederStateChoke>` | `BtSeederStateChoke` | Inline ownership |
//! | `unique_ptr<BtLeecherStateChoke>` | `BtLeecherStateChoke` | Inline ownership |

mod constants;
mod peer_entry;
mod peer_storage_trait;
mod storage;
#[cfg(test)]
mod tests;

// Re-export public API to preserve the original import paths.
pub use constants::{
    CHOKE_ROUND_INTERVAL_SECS, MAX_DROPPED_PEERS, MAX_PEER_LIST_SIZE,
    TEMP_PEER_CLEANUP_INTERVAL_SECS, TEMP_REJECT_TIMEOUT_MIN_SECS,
    TEMP_REJECT_TIMEOUT_RANGE_SECS,
};
pub use peer_entry::PeerEntry;
pub use peer_storage_trait::PeerStorage;
pub use storage::DefaultPeerStorage;
