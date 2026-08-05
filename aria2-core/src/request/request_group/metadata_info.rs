//! Metadata provenance attached to request groups.
//!
//! Mirrors aria2's `MetadataInfo`: a metadata source may be backed by a
//! download GID and URI, or may exist only as in-memory data.

use super::GroupId;

/// Provenance for a group created from metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataInfo {
    metadata_gid: Option<GroupId>,
    uri: String,
    metadata_path: Option<String>,
}

impl MetadataInfo {
    /// Create metadata information backed by a download task.
    pub fn new(metadata_gid: GroupId, uri: impl Into<String>) -> Self {
        Self {
            metadata_gid: Some(metadata_gid),
            uri: uri.into(),
            metadata_path: None,
        }
    }

    /// Create metadata information for data that is already in memory.
    pub fn data_only() -> Self {
        Self {
            metadata_gid: None,
            uri: String::new(),
            metadata_path: None,
        }
    }

    /// Returns `true` when the metadata has no recoverable source task.
    pub fn is_data_only(&self) -> bool {
        self.metadata_gid.is_none()
    }

    /// Return the metadata source URI, if one was recorded.
    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// Return the source task GID, if one exists.
    pub fn gid(&self) -> Option<GroupId> {
        self.metadata_gid
    }

    /// Attach the file path used to persist downloaded metadata.
    #[must_use]
    pub fn with_metadata_path(mut self, path: impl Into<String>) -> Self {
        self.metadata_path = Some(path.into());
        self
    }

    /// Return the persisted metadata file path, if known.
    pub fn metadata_path(&self) -> Option<&str> {
        self.metadata_path.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_only_metadata_has_no_gid() {
        let info = MetadataInfo::data_only();
        assert!(info.is_data_only());
        assert!(info.gid().is_none());
        assert!(info.uri().is_empty());
    }

    #[test]
    fn sourced_metadata_preserves_gid_and_uri() {
        let info = MetadataInfo::new(GroupId::new(42), "https://example.test/file.meta4");
        assert!(!info.is_data_only());
        assert_eq!(info.gid(), Some(GroupId::new(42)));
        assert_eq!(info.uri(), "https://example.test/file.meta4");
    }
}
