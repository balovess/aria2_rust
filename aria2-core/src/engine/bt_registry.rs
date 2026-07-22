//! BtRegistry — Global registry for BitTorrent-related components.
//!
//! Maps GID (download ID) to [`BtObject`], which bundles all shared state
//! for a single BitTorrent download: `DownloadContext`, `PieceStorage`,
//! `PeerStorage`, `BtAnnounce`, `BtRuntime`, and `BtProgressInfoFile`.
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
//! | `shared_ptr<LpdMessageReceiver>` | `Option<u64>` ID-based reference | Type not yet implemented |
//! | `shared_ptr<UDPTrackerClient>` | `Option<u64>` ID-based reference | Type not yet implemented |
//! | `getNull<T>()` for missing entries | `Option<T>` | Rust-idiomatic null handling |
//! | `OutputIterator` for getAllDownloadContext | `Vec<Arc<DownloadContext>>` | Simpler, Rust-idiomatic API |

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use tracing::trace;

use crate::download::DownloadContext;
use crate::engine::bt_peer_blocklist::BtPeerBlocklist;
use crate::engine::bt_peer_storage::PeerStorage;
use crate::engine::bt_runtime::BtRuntime;
use crate::engine::bt_tracker_comm::BtAnnounce;
use crate::segment::piece_storage::PieceStorage;

// ===========================================================================
// BtObject
// ===========================================================================

/// Bundles all shared state for a single BitTorrent download.
///
/// Each active BT download gets one `BtObject` entry in the [`BtRegistry`].
/// The object holds both directly-owned shared references (`Arc<>`) for types
/// that exist in Rust, and ID-based references (`Option<u64>`) for types
/// not yet implemented.
///
/// # ID-Based References
///
/// Fields like `bt_progress_info_file_id` store an ID that
/// can be used to look up the actual object in a global registry. This avoids
/// tight coupling to types that haven't been ported to Rust yet. Once those
/// types are implemented, these fields can be migrated to direct `Arc<>`
/// references.
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

    /// ID-based reference to the BT progress info file for this download.
    /// BtProgressInfoFile does not yet have a 1:1 Rust equivalent.
    pub bt_progress_info_file_id: Option<u64>,
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
            bt_progress_info_file_id: None,
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
            .field("bt_progress_info_file_id", &self.bt_progress_info_file_id)
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
    bt_progress_info_file_id: Option<u64>,
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

    /// Set the BT progress info file ID.
    pub fn bt_progress_info_file_id(mut self, id: u64) -> Self {
        self.bt_progress_info_file_id = Some(id);
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
            bt_progress_info_file_id: self.bt_progress_info_file_id,
        }
    }
}

// ===========================================================================
// BtRegistry
// ===========================================================================

/// Global registry for BitTorrent-related components.
///
/// Maps GID (download ID) to [`BtObject`]. Also holds global BT settings
/// like TCP/UDP listen ports and references to singleton services
/// (LPD message receiver, UDP tracker client).
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
    /// - Empty pool, no LPD receiver, no UDP tracker client.
    pub fn new() -> Self {
        Self {
            pool: HashMap::new(),
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
    /// Performs a linear scan of all registered `BtObject`s, checking
    /// the info hash stored in each `DownloadContext`'s BT metadata.
    ///
    /// Equivalent to C++ `BtRegistry::getDownloadContext(const string&)`.
    ///
    /// # Implementation Note
    ///
    /// C++ uses `bittorrent::getTorrentAttrs(ctx)->infoHash` to access the
    /// info hash from the `DownloadContext`'s `BitTorrent` attribute.
    /// Rust stores the info hash in the `DownloadContext`'s `bt_info_hash_hex`
    /// field for simplicity, since we don't have a full `TorrentAttribute`
    /// type system yet.
    pub fn get_download_context_by_info_hash(&self, info_hash: &str) -> Option<Arc<DownloadContext>> {
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
    /// Equivalent to C++ `BtRegistry::put(a2_gid_t, unique_ptr<BtObject>)`.
    pub fn put(&mut self, gid: u64, obj: BtObject) {
        trace!(gid, "BtRegistry::put");
        self.pool.insert(gid, obj);
    }

    /// Get a reference to the `BtObject` for the given GID.
    ///
    /// Equivalent to C++ `BtRegistry::get(a2_gid_t)`.
    pub fn get(&self, gid: u64) -> Option<&BtObject> {
        self.pool.get(&gid)
    }

    /// Get a mutable reference to the `BtObject` for the given GID.
    ///
    /// This is a Rust addition — C++ aria2 exposes mutable access through
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
    /// Returns `true` if the entry existed and was removed, `false` otherwise.
    ///
    /// Equivalent to C++ `BtRegistry::remove(a2_gid_t)`.
    pub fn remove(&mut self, gid: u64) -> bool {
        let removed = self.pool.remove(&gid).is_some();
        if removed {
            trace!(gid, "BtRegistry::remove: entry removed");
        } else {
            trace!(gid, "BtRegistry::remove: entry not found");
        }
        removed
    }

    /// Remove all entries from the registry.
    ///
    /// Equivalent to C++ `BtRegistry::removeAll()`.
    pub fn remove_all(&mut self) {
        trace!("BtRegistry::remove_all: clearing {} entries", self.pool.len());
        self.pool.clear();
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

        // Mutate through get_mut — set peer_storage to a real DefaultPeerStorage
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

        // GID exists with download_context — verify Arc identity
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
            .bt_progress_info_file_id(40)
            .build();

        assert!(obj.download_context.is_some());
        assert!(obj.piece_storage.is_none());
        assert!(obj.peer_storage.is_some());
        assert!(obj.bt_announce.is_some());
        assert_eq!(obj.bt_progress_info_file_id, Some(40));
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
        assert!(obj.bt_progress_info_file_id.is_none());

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
    // 13. get_download_context_by_info_hash placeholder
    // -----------------------------------------------------------------------

    #[test]
    fn test_get_download_context_by_info_hash_returns_none() {
        // No TorrentAttribute set → returns None
        let mut registry = BtRegistry::new();
        let obj = make_bt_object_with_ctx(1024, 4096, "/tmp/file.bin");
        registry.put(1, obj);

        assert!(registry.get_download_context_by_info_hash("any_hash").is_none());
    }

    #[test]
    fn test_get_download_context_by_info_hash_with_torrent_attribute() {
        use crate::download::download_context::{ContextAttributeType, TorrentAttribute};

        let info_hash = "0123456789abcdef0123456789abcdef01234567";
        let ta = TorrentAttribute::new(info_hash.to_string());

        // Create a DownloadContext with TorrentAttribute set
        let mut ctx = DownloadContext::new(1024, 4096, "/tmp/file.bin".to_string());
        ctx.set_attribute(ContextAttributeType::BitTorrent, Box::new(ta));
        let ctx = Arc::new(ctx);

        let mut registry = BtRegistry::new();
        let obj = BtObject::builder()
            .download_context(Arc::clone(&ctx))
            .build();
        registry.put(1, obj);

        // Lookup by info_hash should find the context
        let found = registry.get_download_context_by_info_hash(info_hash);
        assert!(found.is_some());

        // Wrong hash should not find it
        assert!(registry.get_download_context_by_info_hash("wrong_hash").is_none());
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
        assert!(obj.bt_progress_info_file_id.is_none());
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
}
