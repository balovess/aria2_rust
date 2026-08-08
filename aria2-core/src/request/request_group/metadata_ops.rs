//! Metadata provenance accessors for RequestGroup.

use super::{MetadataInfo, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

impl RequestGroup {
    /// Mark this group as an in-memory source download.
    ///
    /// Mirrors C++ `RequestGroup::markInMemoryDownload()` and is intentionally
    /// independent of whether the bytes are BitTorrent or Metalink data.
    pub fn mark_in_memory_download(&self) {
        self.in_memory_download
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Return whether the source download is memory-backed.
    pub fn is_in_memory_download(&self) -> bool {
        self.in_memory_download
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Store the completed source bytes for a post-download handler.
    pub fn set_in_memory_data(&self, data: Vec<u8>) {
        self.mark_in_memory_download();
        *self.in_memory_data.recover_mut() = Some(data);
    }

    /// Return a snapshot of the completed in-memory source bytes.
    pub fn in_memory_data(&self) -> Option<Vec<u8>> {
        self.in_memory_data.recover().clone()
    }

    /// Record the response Content-Type used by post-download criteria.
    pub fn set_content_type(&self, content_type: impl Into<String>) {
        *self.content_type.recover_mut() = Some(content_type.into());
    }

    /// Return the response Content-Type, if one was observed.
    pub fn content_type(&self) -> Option<String> {
        self.content_type.recover().clone()
    }

    #[cfg(feature = "bittorrent")]
    pub fn set_bt_metadata_data(&self, data: Vec<u8>) {
        *self.bt_metadata_data.recover_mut() = Some(data);
    }

    #[cfg(feature = "bittorrent")]
    pub fn bt_metadata_data(&self) -> Option<Vec<u8>> {
        self.bt_metadata_data.recover().clone()
    }

    /// Attach metadata provenance to this group.
    pub fn set_metadata_info(&self, info: MetadataInfo) {
        *self.metadata_info.recover_mut() = Some(info);
    }

    /// Return a snapshot of metadata provenance, if present.
    pub fn metadata_info(&self) -> Option<MetadataInfo> {
        self.metadata_info.recover().clone()
    }

    #[cfg(feature = "metalink")]
    pub fn set_metalink_source(&self, data: Vec<u8>, file_index: usize) {
        *self.metalink_data.recover_mut() = Some(data);
        *self.metalink_file_index.recover_mut() = Some(file_index);
    }

    #[cfg(feature = "metalink")]
    pub fn set_metalink_base_uri(&self, base_uri: Option<&str>) {
        *self.metalink_base_uri.recover_mut() = base_uri.map(str::to_owned);
    }

    #[cfg(feature = "metalink")]
    pub fn metalink_source(&self) -> Option<(Vec<u8>, usize)> {
        Some((
            self.metalink_data.recover().clone()?,
            (*self.metalink_file_index.recover())?,
        ))
    }

    #[cfg(feature = "metalink")]
    pub fn metalink_base_uri(&self) -> Option<String> {
        self.metalink_base_uri.recover().clone()
    }
}
