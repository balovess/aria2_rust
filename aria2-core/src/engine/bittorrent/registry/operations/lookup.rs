//! BitTorrent registry lookup operations.

use super::super::BtRegistry;
use crate::download::DownloadContext;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

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

    /// Return the configured local peer endpoint for an active torrent when
    /// its info hash and the request's address family match.
    pub fn dht_local_peer(&self, info_hash: &[u8; 20], requester_ip: IpAddr) -> Option<SocketAddr> {
        if self.tcp_port == 0 {
            return None;
        }
        let gid = self.info_hash_index.get(&hex::encode(info_hash))?;
        let external_ip = *self.dht_external_ips.get(gid)?;
        (external_ip.is_ipv4() == requester_ip.is_ipv4())
            .then(|| SocketAddr::new(external_ip, self.tcp_port))
    }

    /// Publish or clear the external address associated with one registered
    /// torrent. Invalid/unconfigured addresses are represented by `None`.
    pub fn set_dht_external_ip(&mut self, gid: u64, external_ip: Option<IpAddr>) {
        if !self.pool.contains_key(&gid) {
            return;
        }
        if let Some(external_ip) = external_ip {
            self.dht_external_ips.insert(gid, external_ip);
        } else {
            self.dht_external_ips.remove(&gid);
        }
    }

    // -----------------------------------------------------------------------
}
