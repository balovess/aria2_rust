//! Stopped (completed/failed/removed) download result storage.
//!
//! Mirrors C++ `RequestGroupMan::downloadResults_`. Stores `DownloadResult`
//! entries for RPC `aria2.tellStopped` / `aria2.getDownloadResult` queries.

use crate::request::request_group::download_result::DownloadResult;
use crate::util::rwlock_ext::RwLockRecover;

/// Storage for completed/failed/removed download results.
pub struct StoppedResults {
    results: std::sync::RwLock<Vec<DownloadResult>>,
}

impl StoppedResults {
    /// Create empty result storage.
    pub fn new() -> Self {
        Self {
            results: std::sync::RwLock::new(Vec::new()),
        }
    }

    /// Add a download result.
    pub fn add(&self, result: DownloadResult) {
        self.results.recover_mut().push(result);
    }

    /// Number of stored results.
    pub fn len(&self) -> usize {
        self.results.recover().len()
    }

    /// Whether there are no stored results.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.results.recover().is_empty()
    }

    /// Find a result by GID hex string.
    pub fn find_by_hex(&self, hex: &str) -> Option<DownloadResult> {
        self.results
            .recover()
            .iter()
            .find(|r| r.gid_hex() == hex)
            .cloned()
    }

    /// Remove a result by GID hex string.
    pub fn remove_by_hex(&self, hex: &str) -> Option<DownloadResult> {
        let mut results = self.results.recover_mut();
        let pos = results.iter().position(|r| r.gid_hex() == hex)?;
        Some(results.remove(pos))
    }

    /// Purge all stored results.
    /// Mirrors C++ `RequestGroupMan::purgeDownloadResult()`.
    pub fn purge_all(&self) -> usize {
        let mut results = self.results.recover_mut();
        let count = results.len();
        results.clear();
        count
    }

    /// Remove the oldest (first) N results.
    /// Mirrors C++ `RequestGroupMan::purgeDownloadResult()` with limit.
    /// Used by housekeeping to enforce `MAX_DOWNLOAD_RESULT`.
    pub fn remove_oldest(&self, count: usize) -> usize {
        let mut results = self.results.recover_mut();
        let to_remove = count.min(results.len());
        results.drain(..to_remove);
        to_remove
    }

    /// Get results in the given range (offset, count).
    /// Returns a snapshot for RPC `tellStopped` pagination.
    /// Supports negative offset for reverse pagination (C++ `getPaginationRange`).
    pub fn get_range(&self, offset: i32, count: usize) -> Vec<DownloadResult> {
        let results = self.results.recover();
        let len = results.len() as i32;

        let start = if offset >= 0 {
            offset as usize
        } else {
            // Negative offset: count from the end
            (len + offset).max(0) as usize
        };

        if start >= len as usize {
            return Vec::new();
        }

        results.iter().skip(start).take(count).cloned().collect()
    }

    /// Iterate over all results (read-only snapshot).
    #[allow(dead_code)]
    pub fn iter_snapshot(&self) -> Vec<DownloadResult> {
        self.results.recover().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::request_group::DownloadResultCode;

    fn make_result(gid_val: u64) -> DownloadResult {
        DownloadResult::new(
            crate::request::request_group::GroupId(gid_val),
            crate::request::request_group::DownloadStatus::Complete,
            DownloadResultCode::Finished,
        )
    }

    #[test]
    fn test_add_and_find() {
        let s = StoppedResults::new();
        s.add(make_result(1));
        s.add(make_result(2));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn test_purge() {
        let s = StoppedResults::new();
        s.add(make_result(1));
        s.add(make_result(2));
        assert_eq!(s.purge_all(), 2);
        assert!(s.is_empty());
    }
}
