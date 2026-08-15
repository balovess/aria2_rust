//! Attribute map, timing, Metalink, BT info hash, signature, owner, and network stats.

use std::any::Any;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use tracing::trace;

use crate::download::download_context::DownloadContext;
use crate::download::download_context::net_stat::NetStat;
use crate::download::download_context::types::{ContextAttributeType, Signature, TorrentAttribute};

impl DownloadContext {
    // -----------------------------------------------------------------------
    // BT Info Hash
    // -----------------------------------------------------------------------

    /// Return the BT info hash as a hex string. Empty for non-BT downloads.
    /// Mirrors C++ `DownloadContext::getInfoHash()`.
    pub fn info_hash_hex(&self) -> Option<String> {
        if self.info_hash.is_empty() {
            None
        } else {
            Some(self.info_hash.clone())
        }
    }

    /// Set the BT info hash from a hex string.
    pub fn set_info_hash(&mut self, hash: String) {
        self.info_hash = hash;
    }

    // -----------------------------------------------------------------------
    // Signature
    // -----------------------------------------------------------------------

    /// Return a reference to the optional signature.
    pub fn get_signature(&self) -> Option<&Signature> {
        self.signature.as_ref()
    }

    /// Set the signature, replacing any existing one.
    pub fn set_signature(&mut self, signature: Signature) {
        self.signature = Some(signature);
    }

    // -----------------------------------------------------------------------
    // Owner RequestGroup
    // -----------------------------------------------------------------------

    /// Return the ID of the owning `RequestGroup`, if set.
    pub fn get_owner_request_group_id(&self) -> Option<u64> {
        self.owner_request_group_id
    }

    /// Set the ID of the owning `RequestGroup`.
    pub fn set_owner_request_group_id(&mut self, id: u64) {
        self.owner_request_group_id = Some(id);
    }

    // -----------------------------------------------------------------------
    // Attributes
    // -----------------------------------------------------------------------

    /// Set a typed attribute, replacing any existing one for the same key.
    pub fn set_attribute(&mut self, key: ContextAttributeType, value: Box<dyn Any + Send + Sync>) {
        self.attrs.insert(key, value);
    }

    /// Get a reference to the attribute for the given key.
    ///
    /// Returns `None` if no attribute is set for that key.
    pub fn get_attribute(&self, key: ContextAttributeType) -> Option<&(dyn Any + Send + Sync)> {
        self.attrs.get(&key).map(|b| b.as_ref())
    }

    /// Whether an attribute is set for the given key.
    pub fn has_attribute(&self, key: ContextAttributeType) -> bool {
        self.attrs.contains_key(&key)
    }

    /// Return a reference to the full attribute map.
    pub fn get_attributes(&self) -> &HashMap<ContextAttributeType, Box<dyn Any + Send + Sync>> {
        &self.attrs
    }

    /// Get the BT info hash hex string, if a TorrentAttribute is set.
    ///
    /// Mirrors C++ `bittorrent::getTorrentAttrs(ctx)->infoHash`.
    /// Returns `None` if no BitTorrent attribute is set or it cannot
    /// be downcast to `TorrentAttribute`.
    pub fn get_bt_info_hash_hex(&self) -> Option<String> {
        self.get_attribute(ContextAttributeType::BitTorrent)
            .and_then(|attr| attr.downcast_ref::<TorrentAttribute>())
            .map(|ta| ta.info_hash.clone())
    }

    // -----------------------------------------------------------------------
    // Timing
    // -----------------------------------------------------------------------

    /// Reset the download start time and clear the stop time.
    ///
    /// Records the current instant as the start time and clears the stop
    /// time, preparing for a new download session.
    pub fn reset_download_start_time(&mut self) {
        self.download_stop_time = None;
        self.net_stat.download_start();
        trace!("Download start time reset");
    }

    /// Record the download stop time as now.
    ///
    /// Also marks the network stat as stopped.
    pub fn reset_download_stop_time(&mut self) {
        self.download_stop_time = Some(Instant::now());
        self.net_stat.download_stop();
        trace!("Download stop time recorded");
    }

    /// Return the recorded download stop time.
    pub fn get_download_stop_time(&self) -> Option<Instant> {
        self.download_stop_time
    }

    /// Calculate the session duration.
    ///
    /// Returns the difference between the download start and stop times.
    /// If either is missing, returns `Duration::ZERO`.
    pub fn calculate_session_time(&self) -> Duration {
        self.net_stat.calculate_session_time()
    }

    // -----------------------------------------------------------------------
    // Metalink
    // -----------------------------------------------------------------------

    /// Whether Metalink parsing is accepted from response headers.
    pub fn get_accept_metalink(&self) -> bool {
        self.accept_metalink
    }

    /// Set whether to accept Metalink info from response headers.
    pub fn set_accept_metalink(&mut self, accept: bool) {
        self.accept_metalink = accept;
    }

    // -----------------------------------------------------------------------
    // Network Stats
    // -----------------------------------------------------------------------

    /// Return a reference to the per-download network statistics.
    pub fn get_net_stat(&self) -> &NetStat {
        &self.net_stat
    }

    /// Return a mutable reference to the per-download network statistics.
    pub fn get_net_stat_mut(&mut self) -> &mut NetStat {
        &mut self.net_stat
    }

    /// Update the download byte counter.
    ///
    /// Increments the local `NetStat`. The C++ version also updates the
    /// global `RequestGroupMan` net stat — that will be wired in later
    /// when the back-pointer mechanism is connected.
    pub fn update_download(&mut self, bytes: u64) {
        self.net_stat.update_download(bytes);
        if let Some(global) = self.global_net_stat.get() {
            global.update_download(bytes);
        }
    }

    /// Update the upload byte counter.
    ///
    /// Same dual-update pattern as `update_download`.
    pub fn update_upload_length(&mut self, bytes: u64) {
        self.net_stat.update_upload_length(bytes);
        if let Some(global) = self.global_net_stat.get() {
            global.update_upload_length(bytes);
        }
    }

    /// Update the upload speed.
    pub fn update_upload_speed(&mut self, bytes: u64) {
        self.net_stat.update_upload_speed(bytes);
        if let Some(global) = self.global_net_stat.get() {
            global.update_upload_speed(bytes);
        }
    }

    /// Attach manager-owned aggregate statistics without a raw owner pointer.
    pub(crate) fn set_global_net_stat(
        &self,
        global: std::sync::Arc<crate::request::global_net_stat::GlobalNetStat>,
    ) {
        let _ = self.global_net_stat.set(global);
    }
}
