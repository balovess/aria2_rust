//! BitTorrent registry completion operations.

use super::super::BtRegistry;

impl BtRegistry {
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
}
