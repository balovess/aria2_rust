//! Metadata provenance accessors for RequestGroup.

use super::{MetadataInfo, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

impl RequestGroup {
    /// Mark this group as an in-memory source download.
    ///
    /// This flag is both the pre-download request and the post-download state,
    /// matching C++ `RequestGroup::markInMemoryDownload()`. It is intentionally
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

    /// Snapshot the durable parts of a BitTorrent dependency for session
    /// serializers. Keeping this behind the RequestGroup seam avoids making
    /// persistence code depend on the dependency's private storage layout.
    #[cfg(feature = "bittorrent")]
    pub fn bt_dependency_descriptor(
        &self,
    ) -> Option<(bool, Vec<String>, Vec<super::BtFileMapping>)> {
        let dependency = self.dependency.recover();
        let dependency = dependency.as_ref()?;
        let dependency = dependency.as_any().downcast_ref::<super::BtDependency>()?;
        Some((
            dependency.uses_memory_source(),
            dependency.fallback_uris().to_vec(),
            dependency.file_mappings().to_vec(),
        ))
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

#[cfg(test)]
mod tests {
    use super::RequestGroup;
    use crate::request::request_group::{DownloadOptions, GroupId};

    #[test]
    fn in_memory_source_state_is_explicit_and_retrievable() {
        let group = RequestGroup::new(
            GroupId::new(7),
            vec!["https://example.test/a".into()],
            DownloadOptions::default(),
        );
        assert!(!group.is_in_memory_download());
        assert_eq!(group.in_memory_data(), None);

        group.set_content_type("application/x-bittorrent");
        group.set_in_memory_data(vec![1, 2, 3]);

        assert!(group.is_in_memory_download());
        assert_eq!(group.in_memory_data(), Some(vec![1, 2, 3]));
        assert_eq!(
            group.content_type().as_deref(),
            Some("application/x-bittorrent")
        );
    }
}
