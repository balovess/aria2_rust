//! Metadata provenance accessors for RequestGroup.

use super::{MetadataInfo, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

impl RequestGroup {
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
    pub fn metalink_source(&self) -> Option<(Vec<u8>, usize)> {
        Some((
            self.metalink_data.recover().clone()?,
            (*self.metalink_file_index.recover())?,
        ))
    }
}
