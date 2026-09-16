//! Group construction and insertion into the request-group manager.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::{debug, info, warn};

use super::RequestGroupMan;
#[cfg(all(feature = "metalink", feature = "bittorrent"))]
use crate::engine::metalink_request_graph;
use crate::error::Result;
use crate::request::request_group::{DownloadOptions, DownloadStatus, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

impl RequestGroupMan {
    // ── Group Addition ──────────────────────────────────────────────────

    /// Add a new download group to the reserved queue.
    /// The engine will promote it to active when a slot is available.
    /// Returns the generated GID.
    pub fn add_group(&self, uris: Vec<String>, options: DownloadOptions) -> Result<GroupId> {
        let _lifecycle = self.lifecycle_guard();
        let gid = self.generate_gid();
        let memory_download = uris
            .first()
            .is_some_and(|uri| options.uses_memory_download_for_uri(uri));
        let mut group = RequestGroup::new(gid, uris, options);
        if memory_download {
            group.mark_in_memory_download();
        }
        if group.options().pause {
            group.pause()?;
        }
        let group = Arc::new(std::sync::RwLock::new(group));
        if !self.register_group(Arc::clone(&group)) {
            return Err(crate::error::Aria2Error::DownloadFailed(format!(
                "GID {} already exists",
                gid.to_hex_string()
            )));
        }
        self.reserved.push_back(group);

        info!("Adding download task #{} (reserved)", gid.value());
        debug!(
            "Current reserved: {}, active: {}",
            self.reserved.len(),
            self.active.len()
        );

        Ok(gid)
    }

    /// Add an already-constructed `RequestGroup` (wrapped in `Arc<RwLock>`)
    /// to the reserved queue. Used by `EngineCommand::AddDownload` which
    /// creates the group externally (e.g. from an RPC `addUri` call).
    pub fn add_group_arc(&self, group: Arc<std::sync::RwLock<RequestGroup>>) {
        let _lifecycle = self.lifecycle_guard();
        let gid = group.recover().gid();
        if matches!(
            group.recover().status(),
            DownloadStatus::Complete | DownloadStatus::Error(_) | DownloadStatus::Removed
        ) {
            warn!(gid = gid.value(), "Ignoring stale terminal request group");
            return;
        }
        let memory_download = {
            let group = group.recover();
            group
                .uris()
                .first()
                .is_some_and(|uri| group.options().uses_memory_download_for_uri(uri))
        };
        if memory_download {
            group.recover().mark_in_memory_download();
        }
        if !self.register_group(Arc::clone(&group)) {
            debug!(gid = gid.value(), "Request group is already registered");
            return;
        }
        if group.recover().options().pause
            && let Err(error) = group.recover_mut().pause()
        {
            warn!(gid = gid.value(), %error, "Failed to apply initial pause option");
        }
        self.next_gid
            .fetch_max(gid.value().saturating_add(1), Ordering::SeqCst);
        self.reserved.push_back(group);
        info!(
            "Adding download task #{} (reserved, pre-constructed)",
            gid.value()
        );
    }

    /// Add a fully restored group while preserving its GID and state.
    ///
    /// Unlike `add_group_arc`, this path validates identity and advances the
    /// automatic allocator before queueing the group.
    pub fn add_restored_group(&self, group: Arc<std::sync::RwLock<RequestGroup>>) -> Result<()> {
        let _lifecycle = self.lifecycle_guard();
        let gid = group.recover().gid();
        if !self.register_group(Arc::clone(&group)) {
            return Err(crate::error::Aria2Error::DownloadFailed(format!(
                "GID {} already exists",
                gid.to_hex_string()
            )));
        }

        self.next_gid
            .fetch_max(gid.value().saturating_add(1), Ordering::SeqCst);
        self.reserved.push_back(group);
        info!("Restored download task #{} (reserved)", gid.to_hex_string());
        Ok(())
    }

    /// Insert a batch of groups at the front of the reserved queue.
    ///
    /// Mirrors C++ `RequestGroupMan::insertReservedGroup(0, nextGroups)`:
    /// child groups from `postDownloadProcessing()` are inserted at
    /// position 0 so they are promoted before other waiting downloads.
    pub fn insert_reserved_at_front(&self, groups: Vec<Arc<std::sync::RwLock<RequestGroup>>>) {
        let _lifecycle = self.lifecycle_guard();
        let mut registered = Vec::with_capacity(groups.len());
        for group in groups {
            let gid = group.recover().gid();
            if self.register_group(Arc::clone(&group)) {
                registered.push(group);
            } else {
                warn!(gid = gid.value(), "Ignoring duplicate child request group");
            }
        }
        let count = registered.len();
        self.reserved.insert_front_batch(registered);
        debug!("Inserted {} groups at front of reserved queue", count);
    }

    /// Insert a metadata/payload graph atomically into the reserved queue.
    /// Metadata is queued first so the payload dependency can only resolve
    /// after the prerequisite has been promoted and completed.
    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    /// Add a Metalink metadata/payload request graph atomically.
    pub fn add_metalink_graph(
        &self,
        graph: metalink_request_graph::MetalinkRequestGraph,
    ) -> Result<(GroupId, GroupId)> {
        let _lifecycle = self.lifecycle_guard();
        let metadata_gid = graph.metadata.recover().gid();
        let payload_gid = graph.payload.recover().gid();
        if metadata_gid == payload_gid {
            return Err(crate::error::Aria2Error::DownloadFailed(
                "Metalink graph metadata and payload must have distinct GIDs".to_string(),
            ));
        }
        if !self.register_group(Arc::clone(&graph.metadata)) {
            return Err(crate::error::Aria2Error::DownloadFailed(
                "Metalink graph contains a duplicate GID".to_string(),
            ));
        }
        if !self.register_group(Arc::clone(&graph.payload)) {
            self.unregister_group(metadata_gid);
            return Err(crate::error::Aria2Error::DownloadFailed(
                "Metalink graph contains a duplicate GID".to_string(),
            ));
        }
        self.next_gid.fetch_max(
            metadata_gid
                .value()
                .max(payload_gid.value())
                .saturating_add(1),
            Ordering::SeqCst,
        );
        self.reserved
            .push_back_batch([graph.metadata, graph.payload]);
        info!(
            metadata_gid = metadata_gid.value(),
            payload_gid = payload_gid.value(),
            "Added Metalink request graph"
        );
        Ok((metadata_gid, payload_gid))
    }

    /// Insert a download group under a caller-chosen GID (used by RPC).
    /// Returns `Err` if the GID already exists.
    pub fn add_group_with_gid(
        &self,
        gid: GroupId,
        uris: Vec<String>,
        options: DownloadOptions,
    ) -> Result<()> {
        let _lifecycle = self.lifecycle_guard();
        let memory_download = uris
            .first()
            .is_some_and(|uri| options.uses_memory_download_for_uri(uri));
        let mut group = RequestGroup::new(gid, uris, options);
        if memory_download {
            group.mark_in_memory_download();
        }
        if group.options().pause {
            group.pause()?;
        }
        let group = Arc::new(std::sync::RwLock::new(group));
        if !self.register_group(Arc::clone(&group)) {
            return Err(crate::error::Aria2Error::DownloadFailed(format!(
                "GID {} already exists",
                gid.to_hex_string()
            )));
        }
        // Keep automatically generated GIDs ahead of explicitly assigned ones.
        self.next_gid
            .fetch_max(gid.value().saturating_add(1), Ordering::SeqCst);
        self.reserved.push_back(group);
        info!(
            "Adding download task (RPC) #{} (reserved)",
            gid.to_hex_string()
        );
        Ok(())
    }
}
