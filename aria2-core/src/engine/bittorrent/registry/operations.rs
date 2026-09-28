//! BtRegistry operations — All method implementations for the BT registry.

use std::sync::Arc;

use tracing::trace;

use super::BtObject;
use super::BtRegistry;
use crate::download::DownloadContext;
use crate::engine::bittorrent::dht::engine_set::DhtEngineSet;
use crate::engine::bittorrent::peer::storage::PeerStorage;
use crate::engine::bittorrent::persistence::progress_info_file::BtProgressManager;
use crate::engine::bittorrent::tracker::communication::BtAnnounce;
use crate::segment::piece_storage::PieceStorage;

impl BtRegistry {
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
    pub fn get_download_context_by_info_hash(
        &self,
        info_hash: &str,
    ) -> Option<Arc<DownloadContext>> {
        // Try the secondary index first (O(1))
        if let Some(&gid) = self.info_hash_index.get(info_hash)
            && let Some(obj) = self.pool.get(&gid)
            && let Some(ref ctx) = obj.download_context
        {
            // Verify the index is still consistent
            if ctx.get_bt_info_hash_hex().as_deref() == Some(info_hash) {
                return Some(Arc::clone(ctx));
            }
        }

        // Fallback: linear scan for entries whose index was never populated
        for obj in self.pool.values() {
            if let Some(ref ctx) = obj.download_context
                && ctx.get_bt_info_hash_hex().as_deref() == Some(info_hash)
            {
                return Some(Arc::clone(ctx));
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
        if let Some(ref ctx) = obj.download_context
            && let Some(hash) = ctx.get_bt_info_hash_hex()
        {
            trace!(gid, info_hash = %hash, "BtRegistry::put: updating info_hash index");
            self.info_hash_index.insert(hash, gid);
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
        self.dht_engines.remove(&gid);
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
        trace!(
            "BtRegistry::remove_all: clearing {} entries",
            self.pool.len()
        );
        self.pool.clear();
        self.info_hash_index.clear();
        self.dht_engines.clear();
        self.global_dht_engines = DhtEngineSet::default();
    }

    // -----------------------------------------------------------------------
    // Per-torrent component accessors (convenience)
    // -----------------------------------------------------------------------

    /// Get the `PieceStorage` for the given GID.
    ///
    /// Convenience method that avoids two-level `Option` unwrapping.
    /// Returns `None` if the GID is not registered or has no piece storage.
    pub fn get_piece_storage(&self, gid: u64) -> Option<Arc<dyn PieceStorage>> {
        self.pool
            .get(&gid)
            .and_then(|obj| obj.piece_storage.clone())
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

    /// Get the shared temporary peer rejection state for the given GID.
    pub fn get_peer_rejection(
        &self,
        gid: u64,
    ) -> Option<crate::engine::bittorrent::peer::storage::SharedPeerRejection> {
        self.pool
            .get(&gid)
            .and_then(|obj| obj.peer_rejection.clone())
    }

    /// Get the `PeerStorage` by info hash.
    ///
    /// Combines info_hash secondary index lookup with peer_storage retrieval.
    pub fn get_peer_storage_by_info_hash(&self, info_hash: &str) -> Option<Arc<dyn PeerStorage>> {
        let gid = self.info_hash_index.get(info_hash)?;
        self.pool.get(gid).and_then(|obj| obj.peer_storage.clone())
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
        self.pool
            .get(&gid)
            .and_then(|obj| obj.bt_progress_manager.clone())
    }

    // -----------------------------------------------------------------------
    // DHT engine management
    // -----------------------------------------------------------------------

    /// Set the process-wide DHT engine for its bound address family.
    pub fn set_global_dht_engine(
        &mut self,
        engine: Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>,
    ) {
        trace!("BtRegistry::set_global_dht_engine");
        self.global_dht_engines.insert(engine);
    }

    pub(crate) fn attach_global_dht_engine_for_gid(
        &mut self,
        gid: u64,
        use_ipv6: bool,
    ) -> Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>> {
        let engine = if use_ipv6 {
            self.global_dht_engines.ipv6()
        } else {
            self.global_dht_engines.ipv4()
        }?;
        self.set_dht_engine_for_gid(gid, Arc::clone(&engine));
        Some(engine)
    }

    /// Install the process DHT engine only if one has not already been
    /// published, returning the canonical shared handle in either case.
    ///
    /// Callers hold the registry write lock across this operation, so
    /// concurrent first-use startup converges on one engine rather than
    /// replacing the process engine with whichever startup finishes last.
    pub fn get_or_install_global_dht_engine(
        &mut self,
        candidate: Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>,
    ) -> Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine> {
        if let Some(engine) = self.global_dht_engines.for_peer(candidate.local_addr()) {
            return engine;
        }

        self.global_dht_engines.insert(Arc::clone(&candidate));
        candidate
    }

    /// Start the process DHT engine for this configuration's address family,
    /// or attach the download to the already-running family engine.
    ///
    /// The family startup gate is acquired without holding the registry lock,
    /// then the registry is checked again before binding a UDP socket. This
    /// keeps concurrent torrent starts from creating throwaway DHT engines.
    pub async fn get_or_start_global_dht_engine(
        registry: &Arc<std::sync::RwLock<Self>>,
        gid: u64,
        config: aria2_protocol::bittorrent::dht::engine::DhtEngineConfig,
    ) -> std::io::Result<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>> {
        let use_ipv6 = config.listen_addr.is_some_and(|address| address.is_ipv6());
        let startup_lock = {
            let registry = registry
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(&registry.dht_engine_start_locks[usize::from(use_ipv6)])
        };
        let startup_guard = startup_lock.lock().await;

        let existing = {
            let registry = registry
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if use_ipv6 {
                registry.global_dht_engines.ipv6()
            } else {
                registry.global_dht_engines.ipv4()
            }
        };
        if let Some(engine) = existing {
            registry
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .set_dht_engine_for_gid(gid, Arc::clone(&engine));
            return Ok(engine);
        }

        let candidate = aria2_protocol::bittorrent::dht::engine::DhtEngine::start(config).await?;
        let engine = {
            let mut registry = registry
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let engine = registry.get_or_install_global_dht_engine(Arc::clone(&candidate));
            registry.set_dht_engine_for_gid(gid, Arc::clone(&engine));
            engine
        };

        drop(startup_guard);
        if !Arc::ptr_eq(&candidate, &engine) {
            candidate.shutdown_async().await;
        }
        Ok(engine)
    }

    /// Get the process-wide DHT engine that can contact this peer address.
    pub fn get_global_dht_engine_for_peer(
        &self,
        address: std::net::SocketAddr,
    ) -> Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>> {
        self.global_dht_engines.for_peer(address)
    }

    /// Return whether a command handle refers to the process-wide engine.
    pub fn is_global_dht_engine(
        &self,
        engine: &Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>,
    ) -> bool {
        self.global_dht_engines
            .iter()
            .any(|current| Arc::ptr_eq(current, engine))
    }

    /// Take all process engines during final session shutdown.
    pub fn take_global_dht_engines(
        &mut self,
    ) -> Vec<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>> {
        std::mem::take(&mut self.global_dht_engines).into_vec()
    }

    /// Register the DHT engine owned by one active download.
    pub fn set_dht_engine_for_gid(
        &mut self,
        gid: u64,
        engine: Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>,
    ) {
        trace!(gid, "BtRegistry::set_dht_engine_for_gid");
        self.dht_engines.entry(gid).or_default().insert(engine);
    }

    /// Remove one download's DHT engine only when the handle still matches.
    pub fn clear_dht_engine_for_gid_if(
        &mut self,
        gid: u64,
        engine: &Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>,
    ) {
        if let Some(engines) = self.dht_engines.get_mut(&gid) {
            engines.remove_if(engine);
            if engines.is_empty() {
                self.dht_engines.remove(&gid);
            }
        }
    }

    /// Clone all currently registered DHT engines for a status snapshot.
    pub fn get_dht_engines(&self) -> Vec<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>> {
        let mut engines = Vec::with_capacity(self.dht_engines.len() * 2 + 2);
        for engine in self
            .global_dht_engines
            .iter()
            .chain(self.dht_engines.values().flat_map(DhtEngineSet::iter))
        {
            if !engines.iter().any(|current| Arc::ptr_eq(current, engine)) {
                engines.push(Arc::clone(engine));
            }
        }
        engines
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
    // Peer blocklist
    // -----------------------------------------------------------------------

    /// Get a reference to the peer blocklist.
    ///
    /// Equivalent to C++ `BtRegistry::getPeerBlocklist()`.
    pub fn peer_blocklist(&self) -> &crate::engine::bittorrent::peer::blocklist::BtPeerBlocklist {
        &self.peer_blocklist
    }

    /// Get a mutable reference to the peer blocklist.
    ///
    /// Use this to add or remove blocklist rules.
    pub fn peer_blocklist_mut(
        &mut self,
    ) -> &mut crate::engine::bittorrent::peer::blocklist::BtPeerBlocklist {
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
        if let Some(ref ctx) = old_obj.download_context
            && let Some(hash) = ctx.get_bt_info_hash_hex()
        {
            // Only remove if the index still maps to this GID;
            // a newer put() may have already updated the mapping.
            if self.info_hash_index.get(&hash) == Some(&gid) {
                self.info_hash_index.remove(&hash);
            }
        }
    }

    /// Rebuild the info_hash secondary index from scratch by scanning all
    /// pool entries. Useful after batch operations that bypass `put()`.
    pub fn rebuild_info_hash_index(&mut self) {
        self.info_hash_index.clear();
        for (&gid, obj) in &self.pool {
            if let Some(ref ctx) = obj.download_context
                && let Some(hash) = ctx.get_bt_info_hash_hex()
            {
                self.info_hash_index.insert(hash, gid);
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
