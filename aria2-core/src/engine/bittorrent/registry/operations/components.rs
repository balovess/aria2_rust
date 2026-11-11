//! BitTorrent registry components operations.

use super::super::BtRegistry;
use crate::engine::bittorrent::peer::storage::PeerStorage;
use crate::engine::bittorrent::persistence::progress_info_file::BtProgressManager;
use crate::engine::bittorrent::tracker::communication::BtAnnounce;
use crate::segment::piece_storage::PieceStorage;
use std::sync::Arc;

impl BtRegistry {
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
}
