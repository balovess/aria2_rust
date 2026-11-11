//! BitTorrent registry pool operations.

use std::sync::Arc;

use super::super::{BtObject, BtRegistry};
use crate::download::DownloadContext;
use crate::engine::bittorrent::dht::engine_set::DhtEngineSet;
use tracing::trace;

impl BtRegistry {
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

        // Remove old indexes before adding the replacement. Doing this in the
        // opposite order would let cleanup erase the replacement's same-hash
        // index when a GID is refreshed in place.
        if let Some(old) = self.pool.insert(gid, obj) {
            self.cleanup_info_hash_index(gid, &old);
        }
        self.dht_external_ips.remove(&gid);

        if let Some(ctx) = self
            .pool
            .get(&gid)
            .and_then(|obj| obj.download_context.as_ref())
            && let Some(hash) = ctx.get_bt_info_hash_hex()
        {
            trace!(gid, info_hash = %hash, "BtRegistry::put: updating info_hash index");
            self.info_hash_index.insert(hash, gid);
        }
    }

    /// Register an active torrent without allowing another GID to claim its
    /// info-hash. Generic `put` intentionally keeps its replacement semantics;
    /// task activation uses this atomic check-and-insert boundary instead.
    pub(crate) fn put_unless_info_hash_registered(
        &mut self,
        gid: u64,
        obj: BtObject,
    ) -> Result<(), String> {
        if let Some(info_hash) = obj
            .download_context
            .as_ref()
            .and_then(|context| context.get_bt_info_hash_hex())
            && let Some(owner_gid) = self.info_hash_owner(&info_hash)
            && owner_gid != gid
        {
            return Err(info_hash);
        }

        self.put(gid, obj);
        Ok(())
    }

    fn info_hash_owner(&self, info_hash: &str) -> Option<u64> {
        if let Some(&gid) = self.info_hash_index.get(info_hash)
            && self
                .pool
                .get(&gid)
                .and_then(|object| object.download_context.as_ref())
                .and_then(|context| context.get_bt_info_hash_hex())
                .as_deref()
                == Some(info_hash)
        {
            return Some(gid);
        }

        self.pool.iter().find_map(|(&gid, object)| {
            (object
                .download_context
                .as_ref()
                .and_then(|context| context.get_bt_info_hash_hex())
                .as_deref()
                == Some(info_hash))
            .then_some(gid)
        })
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
            self.dht_external_ips.remove(&gid);
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
        self.dht_external_ips.clear();
        self.dht_engines.clear();
        self.global_dht_engines = DhtEngineSet::default();
    }

    // -----------------------------------------------------------------------

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
