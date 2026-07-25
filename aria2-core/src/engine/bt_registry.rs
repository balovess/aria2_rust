//! BtRegistry — Global registry for BitTorrent-related components.
//!
//! Maps GID (download ID) to [`BtObject`], which bundles all shared state
//! for a single BitTorrent download: `DownloadContext`, `PieceStorage`,
//! `PeerStorage`, `BtAnnounce`, `BtRuntime`, and `BtProgressManager`.
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
//! | `shared_ptr<BtRuntime>` | `Arc<BtRuntime>` | Same shared-ownership semantics |
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

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use tracing::trace;

use crate::download::DownloadContext;
use crate::engine::bt_peer_blocklist::BtPeerBlocklist;
use crate::engine::bt_peer_storage::PeerStorage;
use crate::engine::bt_progress_info_file::BtProgressManager;
use crate::engine::bt_runtime::BtRuntime;
use crate::engine::bt_tracker_comm::BtAnnounce;
use crate::segment::piece_storage::PieceStorage;

// ===========================================================================
// BtObject
// ===========================================================================

/// Bundles all shared state for a single BitTorrent download.
///
/// Each active BT download gets one `BtObject` entry in the [`BtRegistry`].
/// The object holds shared references (`Arc<>`) for all BT-related components.
///
/// # C++ Equivalence
///
/// | Rust field | C++ field |
/// |---|---|
/// | `download_context` | `shared_ptr<DownloadContext>` |
/// | `piece_storage` | `shared_ptr<PieceStorage>` |
/// | `peer_storage` | `shared_ptr<PeerStorage>` |
/// | `bt_announce` | `shared_ptr<BtAnnounce>` |
/// | `bt_runtime` | `shared_ptr<BtRuntime>` |
/// | `bt_progress_manager` | `shared_ptr<BtProgressInfoFile>` |
pub struct BtObject {
    /// Shared download context (file entries, piece hashes, attributes).
    pub download_context: Option<Arc<DownloadContext>>,

    /// Shared piece storage for this download (trait object for polymorphism).
    /// C++ uses `shared_ptr<PieceStorage>`.
    pub piece_storage: Option<Arc<dyn PieceStorage>>,

    /// Shared peer storage for this download (trait object for polymorphism).
    /// C++ uses `shared_ptr<PeerStorage>`.
    pub peer_storage: Option<Arc<dyn PeerStorage>>,

    /// Shared BT announce handler for this download.
    /// C++ uses `shared_ptr<BtAnnounce>`.
    pub bt_announce: Option<Arc<BtAnnounce>>,

    /// Shared BT runtime state for this download (connection pool, stats).
    pub bt_runtime: Option<Arc<BtRuntime>>,

    /// Shared BT progress manager for this download.
    /// Equivalent to C++ `shared_ptr<BtProgressInfoFile>`.
    pub bt_progress_manager: Option<Arc<BtProgressManager>>,
}

impl BtObject {
    /// Create a new `BtObject` with all fields set to `None`.
    pub fn new() -> Self {
        Self {
            download_context: None,
            piece_storage: None,
            peer_storage: None,
            bt_announce: None,
            bt_runtime: None,
            bt_progress_manager: None,
        }
    }

    /// Return a builder for constructing a `BtObject` with specific fields.
    pub fn builder() -> BtObjectBuilder {
        BtObjectBuilder::default()
    }
}

impl Default for BtObject {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for BtObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BtObject")
            .field(
                "download_context",
                &self
                    .download_context
                    .as_ref()
                    .map(|ctx| format!("<DownloadContext piece_length={}>", ctx.get_piece_length())),
            )
            .field(
                "piece_storage",
                &self.piece_storage.as_ref().map(|_| "<PieceStorage>"),
            )
            .field(
                "peer_storage",
                &self.peer_storage.as_ref().map(|_| "<PeerStorage>"),
            )
            .field(
                "bt_announce",
                &self.bt_announce.as_ref().map(|_| "<BtAnnounce>"),
            )
            .field(
                "bt_runtime",
                &self.bt_runtime.as_ref().map(|_| "<BtRuntime>"),
            )
            .field("bt_progress_manager", &self.bt_progress_manager.as_ref().map(|_| "<BtProgressManager>"))
            .finish()
    }
}

// ===========================================================================
// BtObjectBuilder
// ===========================================================================

/// Builder for constructing a [`BtObject`] with specific fields set.
///
/// # Example
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use aria2_core::engine::bt_registry::BtObject;
/// use aria2_core::download::DownloadContext;
/// use aria2_core::engine::bt_tracker_comm::BtAnnounce;
///
/// let ctx = Arc::new(DownloadContext::new(1024, 4096, "/tmp/file.bin".into()));
/// let bt_announce = Arc::new(BtAnnounce::new(&[], &Some("http://tracker.example/announce".to_string())));
/// let obj = BtObject::builder()
///     .download_context(ctx)
///     .bt_announce(bt_announce)
///     .build();
/// ```
#[derive(Default)]
pub struct BtObjectBuilder {
    download_context: Option<Arc<DownloadContext>>,
    piece_storage: Option<Arc<dyn PieceStorage>>,
    peer_storage: Option<Arc<dyn PeerStorage>>,
    bt_announce: Option<Arc<BtAnnounce>>,
    bt_runtime: Option<Arc<BtRuntime>>,
    bt_progress_manager: Option<Arc<BtProgressManager>>,
}

impl BtObjectBuilder {
    /// Set the download context.
    pub fn download_context(mut self, ctx: Arc<DownloadContext>) -> Self {
        self.download_context = Some(ctx);
        self
    }

    /// Set the piece storage.
    pub fn piece_storage(mut self, storage: Arc<dyn PieceStorage>) -> Self {
        self.piece_storage = Some(storage);
        self
    }

    /// Set the peer storage.
    pub fn peer_storage(mut self, storage: Arc<dyn PeerStorage>) -> Self {
        self.peer_storage = Some(storage);
        self
    }

    /// Set the BT announce handler.
    pub fn bt_announce(mut self, announce: Arc<BtAnnounce>) -> Self {
        self.bt_announce = Some(announce);
        self
    }

    /// Set the BT runtime.
    pub fn bt_runtime(mut self, runtime: Arc<BtRuntime>) -> Self {
        self.bt_runtime = Some(runtime);
        self
    }

    /// Set the BT progress manager.
    pub fn bt_progress_manager(mut self, mgr: Arc<BtProgressManager>) -> Self {
        self.bt_progress_manager = Some(mgr);
        self
    }

    /// Build the `BtObject` from the configured fields.
    pub fn build(self) -> BtObject {
        BtObject {
            download_context: self.download_context,
            piece_storage: self.piece_storage,
            peer_storage: self.peer_storage,
            bt_announce: self.bt_announce,
            bt_runtime: self.bt_runtime,
            bt_progress_manager: self.bt_progress_manager,
        }
    }
}

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
    pool: HashMap<u64, BtObject>,

    /// Secondary index: info_hash hex string -> GID for O(1) lookup.
    /// C++ performs a linear scan over all entries; this index avoids that.
    info_hash_index: HashMap<String, u64>,

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

    // -----------------------------------------------------------------------
    // DownloadContext lookup
    // -----------------------------------------------------------------------

    /// Get the `DownloadContext` for the given GID.
    ///
    /// Returns `None` if the GID is not in the registry or if the
    /// `BtObject` has no `download_context` set.
    ///
    /// Equivalent to C++ `BtRegistry::getDownloadContext(a2_gid_t)`.
    pub fn get_download_context(&self, gid: u64) -> Option<Arc<DownloadContext>> {
        let obj = self.pool.get(&gid)?;
        obj.download_context.clone()
    }

    /// Get the `DownloadContext` by torrent info hash.
    ///
    /// Uses the secondary index for O(1) lookup. Falls back to a linear
    /// scan of all entries if the index has no entry for the given hash
    /// (covers entries registered before the index was populated).
    ///
    /// Equivalent to C++ `BtRegistry::getDownloadContext(const string&)`.
    ///
    /// # Implementation Note
    ///
    /// C++ uses `bittorrent::getTorrentAttrs(ctx)->infoHash` to access the
    /// info hash from the `DownloadContext`'s `BitTorrent` attribute.
    /// Rust uses a secondary `HashMap<String, u64>` index for O(1) lookup
    /// instead of the C++ linear scan.
    pub fn get_download_context_by_info_hash(&self, info_hash: &str) -> Option<Arc<DownloadContext>> {
        // Try the secondary index first (O(1))
        if let Some(&gid) = self.info_hash_index.get(info_hash) {
            if let Some(obj) = self.pool.get(&gid) {
                if let Some(ref ctx) = obj.download_context {
                    // Verify the index is still consistent
                    if ctx.get_bt_info_hash_hex().as_deref() == Some(info_hash) {
                        return Some(Arc::clone(ctx));
                    }
                }
            }
        }

        // Fallback: linear scan for entries whose index was never populated
        for obj in self.pool.values() {
            if let Some(ref ctx) = obj.download_context {
                if ctx.get_bt_info_hash_hex().as_deref() == Some(info_hash) {
                    return Some(Arc::clone(ctx));
                }
            }
        }
        None
    }

    // -----------------------------------------------------------------------
    // Pool operations
    // -----------------------------------------------------------------------

    /// Insert a `BtObject` for the given GID, replacing any existing entry.
    ///
    /// If the `BtObject` has a `download_context` with a BitTorrent attribute
    /// containing an info hash, the secondary index is updated automatically.
    ///
    /// Equivalent to C++ `BtRegistry::put(a2_gid_t, unique_ptr<BtObject>)`.
    pub fn put(&mut self, gid: u64, obj: BtObject) {
        trace!(gid, "BtRegistry::put");

        // Update secondary index if the new object has an info hash
        if let Some(ref ctx) = obj.download_context {
            if let Some(hash) = ctx.get_bt_info_hash_hex() {
                trace!(gid, info_hash = %hash, "BtRegistry::put: updating info_hash index");
                self.info_hash_index.insert(hash, gid);
            }
        }

        // If replacing an existing entry, clean up its stale info_hash index
        if let Some(old) = self.pool.insert(gid, obj) {
            self.cleanup_info_hash_index(gid, &old);
        }
    }

    /// Get a reference to the `BtObject` for the given GID.
    ///
    /// Equivalent to C++ `BtRegistry::get(a2_gid_t)`.
    pub fn get(&self, gid: u64) -> Option<&BtObject> {
        self.pool.get(&gid)
    }

    /// Get a mutable reference to the `BtObject` for the given GID.
    ///
    /// This is a Rust addition -- C++ aria2 exposes mutable access through
    /// the `unique_ptr<BtObject>` directly.
    pub fn get_mut(&mut self, gid: u64) -> Option<&mut BtObject> {
        self.pool.get_mut(&gid)
    }

    /// Collect all `DownloadContext` references from registered objects.
    ///
    /// Equivalent to C++ `BtRegistry::getAllDownloadContext(OutputIterator)`.
    /// Returns only objects that have a `download_context` set.
    pub fn all_download_contexts(&self) -> Vec<Arc<DownloadContext>> {
        self.pool
            .values()
            .filter_map(|obj| obj.download_context.clone())
            .collect()
    }

    /// Remove the `BtObject` for the given GID.
    ///
    /// Also removes the corresponding entry from the info_hash secondary
    /// index if present.
    ///
    /// Returns `true` if the entry existed and was removed, `false` otherwise.
    ///
    /// Equivalent to C++ `BtRegistry::remove(a2_gid_t)`.
    pub fn remove(&mut self, gid: u64) -> bool {
        if let Some(old) = self.pool.remove(&gid) {
            self.cleanup_info_hash_index(gid, &old);
            trace!(gid, "BtRegistry::remove: entry removed");
            true
        } else {
            trace!(gid, "BtRegistry::remove: entry not found");
            false
        }
    }

    /// Remove all entries from the registry.
    ///
    /// Equivalent to C++ `BtRegistry::removeAll()`.
    pub fn remove_all(&mut self) {
        trace!("BtRegistry::remove_all: clearing {} entries", self.pool.len());
        self.pool.clear();
        self.info_hash_index.clear();
    }

    // -----------------------------------------------------------------------
    // Per-torrent component accessors (convenience)
    // -----------------------------------------------------------------------

    /// Get the `PieceStorage` for the given GID.
    ///
    /// Convenience method that avoids two-level `Option` unwrapping.
    /// Returns `None` if the GID is not registered or has no piece storage.
    pub fn get_piece_storage(&self, gid: u64) -> Option<Arc<dyn PieceStorage>> {
        self.pool.get(&gid).and_then(|obj| obj.piece_storage.clone())
    }

    /// Get the `PieceStorage` by info hash.
    ///
    /// Combines info_hash secondary index lookup with piece_storage retrieval.
    pub fn get_piece_storage_by_info_hash(&self, info_hash: &str) -> Option<Arc<dyn PieceStorage>> {
        let gid = self.info_hash_index.get(info_hash)?;
        self.pool.get(gid).and_then(|obj| obj.piece_storage.clone())
    }

    /// Get the `PeerStorage` for the given GID.
    ///
    /// Convenience method that avoids two-level `Option` unwrapping.
    /// Returns `None` if the GID is not registered or has no peer storage.
    pub fn get_peer_storage(&self, gid: u64) -> Option<Arc<dyn PeerStorage>> {
        self.pool.get(&gid).and_then(|obj| obj.peer_storage.clone())
    }

    /// Get the `PeerStorage` by info hash.
    ///
    /// Combines info_hash secondary index lookup with peer_storage retrieval.
    pub fn get_peer_storage_by_info_hash(&self, info_hash: &str) -> Option<Arc<dyn PeerStorage>> {
        let gid = self.info_hash_index.get(info_hash)?;
        self.pool.get(gid).and_then(|obj| obj.peer_storage.clone())
    }

    /// Get the `BtRuntime` for the given GID.
    ///
    /// Returns `None` if the GID is not registered or has no runtime.
    pub fn get_bt_runtime(&self, gid: u64) -> Option<Arc<BtRuntime>> {
        self.pool.get(&gid).and_then(|obj| obj.bt_runtime.clone())
    }

    /// Get the `BtAnnounce` for the given GID.
    ///
    /// Returns `None` if the GID is not registered or has no announce handler.
    pub fn get_bt_announce(&self, gid: u64) -> Option<Arc<BtAnnounce>> {
        self.pool.get(&gid).and_then(|obj| obj.bt_announce.clone())
    }

    /// Get the `BtProgressManager` for the given GID.
    ///
    /// Returns `None` if the GID is not registered or has no progress manager.
    pub fn get_bt_progress_manager(&self, gid: u64) -> Option<Arc<BtProgressManager>> {
        self.pool.get(&gid).and_then(|obj| obj.bt_progress_manager.clone())
    }

    // -----------------------------------------------------------------------
    // DHT engine management
    // -----------------------------------------------------------------------

    /// Set the shared DHT engine for this session.
    ///
    /// In C++ aria2, the DHT node is a process-level singleton accessed
    /// via `DHT::getInstance()`. Here the engine is explicitly set on the
    /// registry, making it testable and lifecycle-managed.
    pub fn set_dht_engine(&mut self, engine: Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>) {
        trace!("BtRegistry::set_dht_engine");
        self.dht_engine = Some(engine);
    }

    /// Get a reference to the shared DHT engine.
    ///
    /// Returns `None` if no DHT engine has been set.
    pub fn get_dht_engine(&self) -> Option<&Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>> {
        self.dht_engine.as_ref()
    }

    /// Remove the DHT engine reference.
    ///
    /// Called during shutdown to release the engine.
    pub fn clear_dht_engine(&mut self) {
        trace!("BtRegistry::clear_dht_engine");
        self.dht_engine = None;
    }

    // -----------------------------------------------------------------------
    // Download completion status
    // -----------------------------------------------------------------------

    /// Check whether all registered downloads are finished.
    ///
    /// A download is considered finished if its `PieceStorage` reports
    /// `is_finished() == true`. Downloads without a `PieceStorage` are
    /// treated as not finished (conservative default).
    ///
    /// Returns `true` if the registry is empty or all downloads are finished.
    ///
    /// This is a Rust addition -- C++ aria2 checks this condition at the
    /// `DownloadEngine` level via `RequestGroupMan::downloadFinished()`.
    /// Here we provide a BT-specific equivalent that checks piece completion
    /// directly from the registry.
    pub fn all_download_finished(&self) -> bool {
        if self.pool.is_empty() {
            return true;
        }
        self.pool.values().all(|obj| {
            obj.piece_storage
                .as_ref()
                .map(|ps| ps.download_finished())
                .unwrap_or(false)
        })
    }

    /// Count how many registered downloads are finished.
    ///
    /// A download is considered finished if its `PieceStorage` reports
    /// `is_finished() == true`.
    pub fn finished_count(&self) -> usize {
        self.pool
            .values()
            .filter(|obj| {
                obj.piece_storage
                    .as_ref()
                    .map(|ps| ps.download_finished())
                    .unwrap_or(false)
            })
            .count()
    }

    // -----------------------------------------------------------------------
    // Torrent attribute convenience methods
    // -----------------------------------------------------------------------

    /// Check whether the torrent identified by `gid` is a private torrent.
    ///
    /// Private torrents disable DHT, PEX, and LPD per BEP 0027.
    /// Returns `false` if the GID is not registered, has no DownloadContext,
    /// or has no BitTorrent attribute.
    pub fn is_private_torrent(&self, gid: u64) -> bool {
        self.pool
            .get(&gid)
            .and_then(|obj| obj.download_context.as_ref())
            .map(|ctx| {
                use crate::download::download_context::{ContextAttributeType, TorrentAttribute};
                ctx.get_attribute(ContextAttributeType::BitTorrent)
                    .and_then(|attr| attr.downcast_ref::<TorrentAttribute>())
                    .map(|ta| ta.private_torrent)
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    /// Get the torrent name for the given GID.
    ///
    /// Returns `None` if the GID is not registered, has no DownloadContext,
    /// or has no BitTorrent attribute with a name.
    pub fn get_torrent_name(&self, gid: u64) -> Option<String> {
        self.pool
            .get(&gid)?
            .download_context
            .as_ref()
            .and_then(|ctx| {
                use crate::download::download_context::{ContextAttributeType, TorrentAttribute};
                ctx.get_attribute(ContextAttributeType::BitTorrent)
                    .and_then(|attr| attr.downcast_ref::<TorrentAttribute>())
                    .map(|ta| ta.name.clone())
            })
    }

    // -----------------------------------------------------------------------
    // Port configuration
    // -----------------------------------------------------------------------

    /// Set the TCP listen port for incoming BitTorrent connections.
    pub fn set_tcp_port(&mut self, port: u16) {
        trace!(port, "BtRegistry::set_tcp_port");
        self.tcp_port = port;
    }

    /// Get the TCP listen port.
    pub fn tcp_port(&self) -> u16 {
        self.tcp_port
    }

    /// Set the UDP port for DHT and UDP tracker.
    pub fn set_udp_port(&mut self, port: u16) {
        trace!(port, "BtRegistry::set_udp_port");
        self.udp_port = port;
    }

    /// Get the UDP port.
    pub fn udp_port(&self) -> u16 {
        self.udp_port
    }

    // -----------------------------------------------------------------------
    // Singleton service references
    // -----------------------------------------------------------------------

    /// Set the ID-based reference to the LPD message receiver.
    pub fn set_lpd_message_receiver_id(&mut self, id: u64) {
        trace!(id, "BtRegistry::set_lpd_message_receiver_id");
        self.lpd_message_receiver_id = Some(id);
    }

    /// Get the ID-based reference to the LPD message receiver.
    pub fn lpd_message_receiver_id(&self) -> Option<u64> {
        self.lpd_message_receiver_id
    }

    /// Set the ID-based reference to the UDP tracker client.
    pub fn set_udp_tracker_client_id(&mut self, id: u64) {
        trace!(id, "BtRegistry::set_udp_tracker_client_id");
        self.udp_tracker_client_id = Some(id);
    }

    /// Get the ID-based reference to the UDP tracker client.
    pub fn udp_tracker_client_id(&self) -> Option<u64> {
        self.udp_tracker_client_id
    }

    // -----------------------------------------------------------------------
    // Peer blocklist
    // -----------------------------------------------------------------------

    /// Get a reference to the peer blocklist.
    ///
    /// Equivalent to C++ `BtRegistry::getPeerBlocklist()`.
    pub fn peer_blocklist(&self) -> &BtPeerBlocklist {
        &self.peer_blocklist
    }

    /// Get a mutable reference to the peer blocklist.
    ///
    /// Use this to add or remove blocklist rules.
    pub fn peer_blocklist_mut(&mut self) -> &mut BtPeerBlocklist {
        &mut self.peer_blocklist
    }

    /// Convenience method: check if a peer IP address is in the blocklist.
    ///
    /// Returns `true` if the address matches any blocked range.
    pub fn is_peer_blocked(&self, addr: &str) -> bool {
        self.peer_blocklist.contains(addr)
    }

    // -----------------------------------------------------------------------
    // Info hash index maintenance (internal)
    // -----------------------------------------------------------------------

    /// Remove the info_hash index entry associated with `gid` if it still
    /// points to this GID. This prevents stale index entries when a GID is
    /// replaced or removed.
    fn cleanup_info_hash_index(&mut self, gid: u64, old_obj: &BtObject) {
        if let Some(ref ctx) = old_obj.download_context {
            if let Some(hash) = ctx.get_bt_info_hash_hex() {
                // Only remove if the index still maps to this GID;
                // a newer put() may have already updated the mapping.
                if self.info_hash_index.get(&hash) == Some(&gid) {
                    self.info_hash_index.remove(&hash);
                }
            }
        }
    }

    /// Rebuild the info_hash secondary index from scratch by scanning all
    /// pool entries. Useful after batch operations that bypass `put()`.
    pub fn rebuild_info_hash_index(&mut self) {
        self.info_hash_index.clear();
        for (&gid, obj) in &self.pool {
            if let Some(ref ctx) = obj.download_context {
                if let Some(hash) = ctx.get_bt_info_hash_hex() {
                    self.info_hash_index.insert(hash, gid);
                }
            }
        }
        trace!(
            index_len = self.info_hash_index.len(),
            pool_len = self.pool.len(),
            "BtRegistry::rebuild_info_hash_index"
        );
    }

    /// Return the number of entries in the info_hash secondary index.
    ///
    /// May differ from `len()` if some BtObjects lack a TorrentAttribute.
    pub fn info_hash_index_len(&self) -> usize {
        self.info_hash_index.len()
    }

    /// Return the number of registered BtObjects.
    pub fn len(&self) -> usize {
        self.pool.len()
    }

    /// Return `true` if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.pool.is_empty()
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

// ===========================================================================
// Unit Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a BtObject with only a DownloadContext.
    fn make_bt_object_with_ctx(piece_length: u32, total_length: u64, path: &str) -> BtObject {
        let ctx = Arc::new(DownloadContext::new(piece_length, total_length, path.to_string()));
        BtObject {
            download_context: Some(ctx),
            ..BtObject::new()
        }
    }

    /// Helper: create a BtObject with DownloadContext and TorrentAttribute.
    fn make_bt_object_with_info_hash(
        piece_length: u32,
        total_length: u64,
        path: &str,
        info_hash: &str,
        private: bool,
    ) -> BtObject {
        use crate::download::download_context::{ContextAttributeType, TorrentAttribute};

        let mut ctx = DownloadContext::new(piece_length, total_length, path.to_string());
        let mut ta = TorrentAttribute::new(info_hash.to_string());
        ta.private_torrent = private;
        ta.name = format!("torrent_{}", info_hash);
        ctx.set_attribute(ContextAttributeType::BitTorrent, Box::new(ta));
        let ctx = Arc::new(ctx);

        BtObject {
            download_context: Some(ctx),
            ..BtObject::new()
        }
    }

    // -----------------------------------------------------------------------
    // 1. put and get
    // -----------------------------------------------------------------------

    #[test]
    fn test_put_and_get() {
        let mut registry = BtRegistry::new();
        assert!(registry.get(1).is_none());

        let obj = make_bt_object_with_ctx(1024, 4096, "/tmp/file.bin");
        registry.put(1, obj);

        let retrieved = registry.get(1);
        assert!(retrieved.is_some());
        assert!(retrieved.unwrap().download_context.is_some());
    }

    // -----------------------------------------------------------------------
    // 2. get_mut
    // -----------------------------------------------------------------------

    #[test]
    fn test_get_mut() {
        let mut registry = BtRegistry::new();
        let obj = make_bt_object_with_ctx(1024, 4096, "/tmp/file.bin");
        registry.put(1, obj);

        // Mutate through get_mut -- set peer_storage to a real DefaultPeerStorage
        let obj_mut = registry.get_mut(1).unwrap();
        let ps = Arc::new(crate::engine::bt_peer_storage::DefaultPeerStorage::new());
        obj_mut.peer_storage = Some(ps);

        // Verify the mutation is visible
        let obj_ref = registry.get(1).unwrap();
        assert!(obj_ref.peer_storage.is_some());
    }

    #[test]
    fn test_get_mut_nonexistent() {
        let mut registry = BtRegistry::new();
        assert!(registry.get_mut(999).is_none());
    }

    // -----------------------------------------------------------------------
    // 3. remove
    // -----------------------------------------------------------------------

    #[test]
    fn test_remove() {
        let mut registry = BtRegistry::new();

        let obj1 = make_bt_object_with_ctx(1024, 4096, "/tmp/file1.bin");
        let obj2 = make_bt_object_with_ctx(2048, 8192, "/tmp/file2.bin");
        registry.put(1, obj1);
        registry.put(2, obj2);

        // Remove existing entry
        assert!(registry.remove(1));
        assert!(registry.get(1).is_none());
        assert!(registry.get(2).is_some());

        // Remove non-existent entry
        assert!(!registry.remove(1));
    }

    // -----------------------------------------------------------------------
    // 4. remove_all
    // -----------------------------------------------------------------------

    #[test]
    fn test_remove_all() {
        let mut registry = BtRegistry::new();

        let obj1 = make_bt_object_with_ctx(1024, 4096, "/tmp/file1.bin");
        let obj2 = make_bt_object_with_ctx(2048, 8192, "/tmp/file2.bin");
        registry.put(1, obj1);
        registry.put(2, obj2);

        assert_eq!(registry.len(), 2);
        registry.remove_all();
        assert!(registry.is_empty());
        assert!(registry.get(1).is_none());
        assert!(registry.get(2).is_none());
    }

    // -----------------------------------------------------------------------
    // 5. get_download_context
    // -----------------------------------------------------------------------

    #[test]
    fn test_get_download_context() {
        let mut registry = BtRegistry::new();

        // Non-existent GID
        assert!(registry.get_download_context(1).is_none());

        // GID exists but no download_context
        registry.put(1, BtObject::new());
        assert!(registry.get_download_context(1).is_none());

        // GID exists with download_context -- verify Arc identity
        let ctx = Arc::new(DownloadContext::new(1024, 4096, "/tmp/file.bin".into()));
        let mut obj = BtObject::new();
        obj.download_context = Some(Arc::clone(&ctx));
        registry.put(2, obj);

        let result = registry.get_download_context(2);
        assert!(result.is_some());
        // Verify it's the same underlying allocation
        assert!(Arc::ptr_eq(&result.unwrap(), &ctx));
    }

    // -----------------------------------------------------------------------
    // 6. all_download_contexts
    // -----------------------------------------------------------------------

    #[test]
    fn test_all_download_contexts() {
        let mut registry = BtRegistry::new();

        // Empty registry
        assert!(registry.all_download_contexts().is_empty());

        // Two entries with download_context
        let obj1 = make_bt_object_with_ctx(1024, 4096, "/tmp/file1.bin");
        let obj2 = make_bt_object_with_ctx(2048, 8192, "/tmp/file2.bin");
        registry.put(1, obj1);
        registry.put(2, obj2);

        let contexts = registry.all_download_contexts();
        assert_eq!(contexts.len(), 2);

        // One entry without download_context should be skipped
        registry.put(3, BtObject::new());
        let contexts = registry.all_download_contexts();
        assert_eq!(contexts.len(), 2);
    }

    // -----------------------------------------------------------------------
    // 7. tcp_port / udp_port
    // -----------------------------------------------------------------------

    #[test]
    fn test_tcp_udp_port() {
        let mut registry = BtRegistry::new();

        // Default ports are 0
        assert_eq!(registry.tcp_port(), 0);
        assert_eq!(registry.udp_port(), 0);

        registry.set_tcp_port(6881);
        registry.set_udp_port(6882);

        assert_eq!(registry.tcp_port(), 6881);
        assert_eq!(registry.udp_port(), 6882);
    }

    // -----------------------------------------------------------------------
    // 8. Overwrite with put
    // -----------------------------------------------------------------------

    #[test]
    fn test_put_overwrite() {
        let mut registry = BtRegistry::new();

        let obj1 = make_bt_object_with_ctx(1024, 4096, "/tmp/old.bin");
        registry.put(1, obj1);
        assert_eq!(registry.get(1).unwrap().download_context.as_ref().unwrap().get_piece_length(), 1024);

        let obj2 = make_bt_object_with_ctx(2048, 8192, "/tmp/new.bin");
        registry.put(1, obj2);
        assert_eq!(registry.get(1).unwrap().download_context.as_ref().unwrap().get_piece_length(), 2048);
    }

    // -----------------------------------------------------------------------
    // 9. Empty registry operations
    // -----------------------------------------------------------------------

    #[test]
    fn test_empty_registry_operations() {
        let mut registry = BtRegistry::new();

        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.get(1).is_none());
        assert!(registry.get_download_context(1).is_none());
        assert!(registry.all_download_contexts().is_empty());
        assert!(!registry.remove(1));
    }

    // -----------------------------------------------------------------------
    // 10. Builder pattern
    // -----------------------------------------------------------------------

    #[test]
    fn test_bt_object_builder() {
        let ctx = Arc::new(DownloadContext::new(1024, 4096, "/tmp/file.bin".into()));
        let bt_announce = Arc::new(BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string())));
        let peer_storage = Arc::new(crate::engine::bt_peer_storage::DefaultPeerStorage::new());

        let obj = BtObject::builder()
            .download_context(ctx)
            .peer_storage(peer_storage)
            .bt_announce(bt_announce)
            .build();

        assert!(obj.download_context.is_some());
        assert!(obj.piece_storage.is_none());
        assert!(obj.peer_storage.is_some());
        assert!(obj.bt_announce.is_some());
        assert!(obj.bt_progress_manager.is_none());
        assert!(obj.bt_runtime.is_none());
    }

    // -----------------------------------------------------------------------
    // 11. Default trait
    // -----------------------------------------------------------------------

    #[test]
    fn test_default() {
        let obj = BtObject::default();
        assert!(obj.download_context.is_none());
        assert!(obj.piece_storage.is_none());
        assert!(obj.peer_storage.is_none());
        assert!(obj.bt_announce.is_none());
        assert!(obj.bt_runtime.is_none());
        assert!(obj.bt_progress_manager.is_none());

        let registry = BtRegistry::default();
        assert!(registry.is_empty());
        assert_eq!(registry.tcp_port(), 0);
        assert_eq!(registry.udp_port(), 0);
        assert!(registry.lpd_message_receiver_id().is_none());
        assert!(registry.udp_tracker_client_id().is_none());
    }

    // -----------------------------------------------------------------------
    // 12. Singleton service IDs
    // -----------------------------------------------------------------------

    #[test]
    fn test_singleton_service_ids() {
        let mut registry = BtRegistry::new();

        assert!(registry.lpd_message_receiver_id().is_none());
        assert!(registry.udp_tracker_client_id().is_none());

        registry.set_lpd_message_receiver_id(100);
        registry.set_udp_tracker_client_id(200);

        assert_eq!(registry.lpd_message_receiver_id(), Some(100));
        assert_eq!(registry.udp_tracker_client_id(), Some(200));
    }

    // -----------------------------------------------------------------------
    // 13. get_download_context_by_info_hash with secondary index
    // -----------------------------------------------------------------------

    #[test]
    fn test_get_download_context_by_info_hash_returns_none_without_attribute() {
        let mut registry = BtRegistry::new();
        let obj = make_bt_object_with_ctx(1024, 4096, "/tmp/file.bin");
        registry.put(1, obj);

        assert!(registry.get_download_context_by_info_hash("any_hash").is_none());
    }

    #[test]
    fn test_get_download_context_by_info_hash_with_torrent_attribute() {
        let info_hash = "0123456789abcdef0123456789abcdef01234567";
        let obj = make_bt_object_with_info_hash(1024, 4096, "/tmp/file.bin", info_hash, false);

        let mut registry = BtRegistry::new();
        registry.put(1, obj);

        // Lookup by info_hash should find the context via the index
        let found = registry.get_download_context_by_info_hash(info_hash);
        assert!(found.is_some());

        // Wrong hash should not find it
        assert!(registry.get_download_context_by_info_hash("wrong_hash").is_none());

        // Secondary index should be populated
        assert_eq!(registry.info_hash_index_len(), 1);
    }

    // -----------------------------------------------------------------------
    // 14. BtObject new() has all None fields
    // -----------------------------------------------------------------------

    #[test]
    fn test_bt_object_new_all_none() {
        let obj = BtObject::new();
        assert!(obj.download_context.is_none());
        assert!(obj.piece_storage.is_none());
        assert!(obj.peer_storage.is_none());
        assert!(obj.bt_announce.is_none());
        assert!(obj.bt_runtime.is_none());
        assert!(obj.bt_progress_manager.is_none());
    }

    // -----------------------------------------------------------------------
    // 15. Multiple GIDs in registry
    // -----------------------------------------------------------------------

    #[test]
    fn test_multiple_gids() {
        let mut registry = BtRegistry::new();

        for i in 1..=10 {
            let obj = make_bt_object_with_ctx(1024 * i as u32, 4096 * i as u64, &format!("/tmp/file{}.bin", i));
            registry.put(i, obj);
        }

        assert_eq!(registry.len(), 10);
        let contexts = registry.all_download_contexts();
        assert_eq!(contexts.len(), 10);

        // Remove every other entry
        for i in (1..=10).step_by(2) {
            assert!(registry.remove(i));
        }

        assert_eq!(registry.len(), 5);
    }

    // -----------------------------------------------------------------------
    // 16. Peer blocklist integration
    // -----------------------------------------------------------------------

    #[test]
    fn test_registry_has_empty_blocklist_by_default() {
        let registry = BtRegistry::new();
        assert_eq!(registry.peer_blocklist().count(), 0);
        assert!(!registry.is_peer_blocked("10.0.0.1"));
    }

    #[test]
    fn test_registry_blocklist_add_and_check() {
        let mut registry = BtRegistry::new();
        registry.peer_blocklist_mut().add_rule("10.0.0.0/8").unwrap();

        assert!(registry.is_peer_blocked("10.0.0.1"));
        assert!(registry.is_peer_blocked("10.255.255.255"));
        assert!(!registry.is_peer_blocked("192.168.1.1"));
        assert_eq!(registry.peer_blocklist().count(), 1);
    }

    #[test]
    fn test_registry_blocklist_accessors() {
        let mut registry = BtRegistry::new();
        registry.peer_blocklist_mut().add_rule("192.168.0.0/16").unwrap();

        // Immutable accessor
        assert_eq!(registry.peer_blocklist().count(), 1);
        assert!(registry.peer_blocklist().contains("192.168.1.1"));

        // Mutable accessor
        registry.peer_blocklist_mut().clear();
        assert_eq!(registry.peer_blocklist().count(), 0);
    }

    // -----------------------------------------------------------------------
    // 17. DHT engine management
    // -----------------------------------------------------------------------

    #[test]
    fn test_dht_engine_default_none() {
        let registry = BtRegistry::new();
        assert!(registry.get_dht_engine().is_none());
    }

    #[test]
    fn test_dht_engine_set_and_get() {
        use aria2_protocol::bittorrent::dht::engine::{DhtEngine, DhtEngineConfig};

        let rt = tokio::runtime::Runtime::new().unwrap();
        let engine = rt.block_on(async {
            DhtEngine::start(DhtEngineConfig::default()).await.unwrap()
        });

        let mut registry = BtRegistry::new();
        registry.set_dht_engine(engine);

        assert!(registry.get_dht_engine().is_some());
    }

    #[test]
    fn test_dht_engine_clear() {
        use aria2_protocol::bittorrent::dht::engine::{DhtEngine, DhtEngineConfig};

        let rt = tokio::runtime::Runtime::new().unwrap();
        let engine = rt.block_on(async {
            DhtEngine::start(DhtEngineConfig::default()).await.unwrap()
        });

        let mut registry = BtRegistry::new();
        registry.set_dht_engine(engine);
        assert!(registry.get_dht_engine().is_some());

        registry.clear_dht_engine();
        assert!(registry.get_dht_engine().is_none());
    }

    // -----------------------------------------------------------------------
    // 18. Info hash secondary index
    // -----------------------------------------------------------------------

    #[test]
    fn test_info_hash_index_populated_on_put() {
        let hash1 = "aaa1111111111111111111111111111111111111";
        let hash2 = "bbb2222222222222222222222222222222222222";

        let mut registry = BtRegistry::new();
        let obj1 = make_bt_object_with_info_hash(1024, 4096, "/tmp/a.bin", hash1, false);
        let obj2 = make_bt_object_with_info_hash(2048, 8192, "/tmp/b.bin", hash2, false);

        registry.put(1, obj1);
        registry.put(2, obj2);

        assert_eq!(registry.info_hash_index_len(), 2);

        // Lookup by hash1 finds GID 1
        let ctx = registry.get_download_context_by_info_hash(hash1);
        assert!(ctx.is_some());

        // Lookup by hash2 finds GID 2
        let ctx = registry.get_download_context_by_info_hash(hash2);
        assert!(ctx.is_some());
    }

    #[test]
    fn test_info_hash_index_cleaned_on_remove() {
        let hash1 = "aaa1111111111111111111111111111111111111";
        let mut registry = BtRegistry::new();
        let obj1 = make_bt_object_with_info_hash(1024, 4096, "/tmp/a.bin", hash1, false);
        registry.put(1, obj1);

        assert_eq!(registry.info_hash_index_len(), 1);
        assert!(registry.remove(1));
        assert_eq!(registry.info_hash_index_len(), 0);

        // Lookup should now fail
        assert!(registry.get_download_context_by_info_hash(hash1).is_none());
    }

    #[test]
    fn test_info_hash_index_cleaned_on_overwrite() {
        let hash1 = "aaa1111111111111111111111111111111111111";
        let hash2 = "bbb2222222222222222222222222222222222222";

        let mut registry = BtRegistry::new();
        let obj1 = make_bt_object_with_info_hash(1024, 4096, "/tmp/a.bin", hash1, false);
        registry.put(1, obj1);
        assert_eq!(registry.info_hash_index_len(), 1);

        // Overwrite GID 1 with a different info_hash
        let obj2 = make_bt_object_with_info_hash(2048, 8192, "/tmp/b.bin", hash2, false);
        registry.put(1, obj2);

        // hash1 should be gone, hash2 should be present
        assert_eq!(registry.info_hash_index_len(), 1);
        assert!(registry.get_download_context_by_info_hash(hash1).is_none());
        assert!(registry.get_download_context_by_info_hash(hash2).is_some());
    }

    #[test]
    fn test_info_hash_index_cleared_on_remove_all() {
        let hash1 = "aaa1111111111111111111111111111111111111";
        let mut registry = BtRegistry::new();
        let obj1 = make_bt_object_with_info_hash(1024, 4096, "/tmp/a.bin", hash1, false);
        registry.put(1, obj1);

        assert_eq!(registry.info_hash_index_len(), 1);
        registry.remove_all();
        assert_eq!(registry.info_hash_index_len(), 0);
    }

    #[test]
    fn test_rebuild_info_hash_index() {
        let mut registry = BtRegistry::new();
        // Directly insert into pool (bypassing put) -- index stays empty
        let obj1 = make_bt_object_with_info_hash(1024, 4096, "/tmp/a.bin",
            "aaa1111111111111111111111111111111111111", false);
        registry.pool.insert(1, obj1);

        assert_eq!(registry.info_hash_index_len(), 0);

        // Rebuild the index
        registry.rebuild_info_hash_index();
        assert_eq!(registry.info_hash_index_len(), 1);
        assert!(registry.get_download_context_by_info_hash("aaa1111111111111111111111111111111111111").is_some());
    }

    // -----------------------------------------------------------------------
    // 19. Per-torrent convenience accessors
    // -----------------------------------------------------------------------

    #[test]
    fn test_get_piece_storage() {
        let mut registry = BtRegistry::new();
        let mut obj = BtObject::new();
        let ps: Arc<dyn PieceStorage> = Arc::new(
            crate::segment::piece_storage::DefaultPieceStorage::new(1024, 4096),
        );
        obj.piece_storage = Some(ps);
        registry.put(1, obj);

        assert!(registry.get_piece_storage(1).is_some());
        assert!(registry.get_piece_storage(999).is_none());

        // Entry without piece_storage
        registry.put(2, BtObject::new());
        assert!(registry.get_piece_storage(2).is_none());
    }

    #[test]
    fn test_get_peer_storage() {
        let mut registry = BtRegistry::new();
        let mut obj = BtObject::new();
        let ps: Arc<dyn PeerStorage> = Arc::new(
            crate::engine::bt_peer_storage::DefaultPeerStorage::new(),
        );
        obj.peer_storage = Some(ps);
        registry.put(1, obj);

        assert!(registry.get_peer_storage(1).is_some());
        assert!(registry.get_peer_storage(999).is_none());
    }

    #[test]
    fn test_get_bt_runtime() {
        let mut registry = BtRegistry::new();
        let mut obj = BtObject::new();
        obj.bt_runtime = Some(Arc::new(BtRuntime::new()));
        registry.put(1, obj);

        assert!(registry.get_bt_runtime(1).is_some());
        assert!(registry.get_bt_runtime(999).is_none());
    }

    #[test]
    fn test_get_bt_announce() {
        let mut registry = BtRegistry::new();
        let mut obj = BtObject::new();
        obj.bt_announce = Some(Arc::new(BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()))));
        registry.put(1, obj);

        assert!(registry.get_bt_announce(1).is_some());
        assert!(registry.get_bt_announce(999).is_none());
    }

    #[test]
    fn test_get_piece_storage_by_info_hash() {
        let hash1 = "aaa1111111111111111111111111111111111111";
        let mut obj = make_bt_object_with_info_hash(1024, 4096, "/tmp/a.bin", hash1, false);
        let ps: Arc<dyn PieceStorage> = Arc::new(
            crate::segment::piece_storage::DefaultPieceStorage::new(1024, 4096),
        );
        obj.piece_storage = Some(ps);

        let mut registry = BtRegistry::new();
        registry.put(1, obj);

        assert!(registry.get_piece_storage_by_info_hash(hash1).is_some());
        assert!(registry.get_piece_storage_by_info_hash("wrong_hash").is_none());
    }

    #[test]
    fn test_get_peer_storage_by_info_hash() {
        let hash1 = "aaa1111111111111111111111111111111111111";
        let mut obj = make_bt_object_with_info_hash(1024, 4096, "/tmp/a.bin", hash1, false);
        let ps: Arc<dyn PeerStorage> = Arc::new(
            crate::engine::bt_peer_storage::DefaultPeerStorage::new(),
        );
        obj.peer_storage = Some(ps);

        let mut registry = BtRegistry::new();
        registry.put(1, obj);

        assert!(registry.get_peer_storage_by_info_hash(hash1).is_some());
        assert!(registry.get_peer_storage_by_info_hash("wrong_hash").is_none());
    }

    // -----------------------------------------------------------------------
    // 20. all_download_finished
    // -----------------------------------------------------------------------

    #[test]
    fn test_all_download_finished_empty_registry() {
        let registry = BtRegistry::new();
        assert!(registry.all_download_finished(), "empty registry should return true");
    }

    #[test]
    fn test_all_download_finished_no_piece_storage() {
        let mut registry = BtRegistry::new();
        // Entries without piece_storage are treated as not finished
        registry.put(1, BtObject::new());
        assert!(!registry.all_download_finished());
    }

    #[test]
    fn test_all_download_finished_with_finished_storage() {
        use crate::segment::piece_storage::DefaultPieceStorage;

        let mut registry = BtRegistry::new();
        let mut obj = BtObject::new();

        // DefaultPieceStorage with 0 total_length is "finished" (no pieces needed)
        let ps: Arc<dyn PieceStorage> = Arc::new(DefaultPieceStorage::new(0, 0));
        obj.piece_storage = Some(ps);
        registry.put(1, obj);

        assert!(registry.all_download_finished());
    }

    #[test]
    fn test_all_download_finished_with_unfinished_storage() {
        use crate::segment::piece_storage::DefaultPieceStorage;

        let mut registry = BtRegistry::new();
        let mut obj = BtObject::new();

        // 4 pieces, none completed -> not finished
        let ps: Arc<dyn PieceStorage> = Arc::new(DefaultPieceStorage::new(1024, 4096));
        obj.piece_storage = Some(ps);
        registry.put(1, obj);

        assert!(!registry.all_download_finished());
    }

    #[test]
    fn test_finished_count() {
        use crate::segment::piece_storage::DefaultPieceStorage;

        let mut registry = BtRegistry::new();

        // Entry 1: finished (0-length)
        let mut obj1 = BtObject::new();
        obj1.piece_storage = Some(Arc::new(DefaultPieceStorage::new(0, 0)) as Arc<dyn PieceStorage>);
        registry.put(1, obj1);

        // Entry 2: not finished
        let mut obj2 = BtObject::new();
        obj2.piece_storage = Some(Arc::new(DefaultPieceStorage::new(1024, 4096)) as Arc<dyn PieceStorage>);
        registry.put(2, obj2);

        // Entry 3: no piece storage
        registry.put(3, BtObject::new());

        assert_eq!(registry.finished_count(), 1);
    }

    // -----------------------------------------------------------------------
    // 21. Torrent attribute convenience methods
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_private_torrent() {
        let mut registry = BtRegistry::new();

        // Private torrent
        let obj1 = make_bt_object_with_info_hash(1024, 4096, "/tmp/private.bin",
            "aaa1111111111111111111111111111111111111", true);
        registry.put(1, obj1);

        // Public torrent
        let obj2 = make_bt_object_with_info_hash(1024, 4096, "/tmp/public.bin",
            "bbb2222222222222222222222222222222222222", false);
        registry.put(2, obj2);

        // No torrent attribute
        let obj3 = make_bt_object_with_ctx(1024, 4096, "/tmp/noattr.bin");
        registry.put(3, obj3);

        assert!(registry.is_private_torrent(1));
        assert!(!registry.is_private_torrent(2));
        assert!(!registry.is_private_torrent(3));
        assert!(!registry.is_private_torrent(999)); // non-existent
    }

    #[test]
    fn test_get_torrent_name() {
        let mut registry = BtRegistry::new();

        let obj1 = make_bt_object_with_info_hash(1024, 4096, "/tmp/a.bin",
            "aaa1111111111111111111111111111111111111", false);
        registry.put(1, obj1);

        let name = registry.get_torrent_name(1);
        assert!(name.is_some());
        assert!(name.unwrap().starts_with("torrent_"));

        // No torrent attribute
        let obj2 = make_bt_object_with_ctx(1024, 4096, "/tmp/noattr.bin");
        registry.put(2, obj2);
        assert!(registry.get_torrent_name(2).is_none());

        // Non-existent
        assert!(registry.get_torrent_name(999).is_none());
    }

    // -----------------------------------------------------------------------
    // 22. Debug output includes new fields
    // -----------------------------------------------------------------------

    #[test]
    fn test_debug_includes_dht_and_index() {
        let registry = BtRegistry::new();
        let debug_str = format!("{:?}", registry);
        assert!(debug_str.contains("has_dht_engine: false"));
        assert!(debug_str.contains("info_hash_index_len: 0"));
    }

    // -----------------------------------------------------------------------
    // 23. Multiple info_hash lookups with same hash (collision)
    // -----------------------------------------------------------------------

    #[test]
    fn test_info_hash_index_last_writer_wins() {
        let hash = "aaa1111111111111111111111111111111111111";

        let mut registry = BtRegistry::new();

        // First put with GID 1
        let obj1 = make_bt_object_with_info_hash(1024, 4096, "/tmp/a.bin", hash, false);
        registry.put(1, obj1);
        assert_eq!(registry.info_hash_index.get(hash), Some(&1));

        // Second put with GID 2 using the SAME info_hash -- last writer wins
        let obj2 = make_bt_object_with_info_hash(2048, 8192, "/tmp/b.bin", hash, false);
        registry.put(2, obj2);
        assert_eq!(registry.info_hash_index.get(hash), Some(&2));

        // Lookup should return the GID 2 context
        let ctx = registry.get_download_context_by_info_hash(hash);
        assert!(ctx.is_some());
        assert_eq!(ctx.unwrap().get_piece_length(), 2048);

        // GID 1 is still in the pool but no longer indexed by hash
        assert!(registry.get(1).is_some());
    }
}
