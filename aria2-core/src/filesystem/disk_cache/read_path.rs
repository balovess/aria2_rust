use tracing::debug;

use crate::error::Result;
use super::WrDiskCache;

impl WrDiskCache {
    /// Read cached data at the given offset and length.
    ///
    /// Returns Some(data) if the requested range is fully contained in a cached entry,
    /// or None if the data is not in the cache.
    ///
    /// # Complexity
    /// O(log n) -- a single BTreeMap::range lookup finds the unique candidate
    /// entry (the one with the largest start key <= offset). If that entry
    /// does not fully cover [offset, offset+length), no other entry can
    /// (entries with smaller keys end before offset; entries with larger keys
    /// start after offset).
    ///
    /// # Zero-Copy
    /// Returns bytes::Bytes slice instead of Vec<u8>, avoiding memory allocation.
    pub async fn read(&self, offset: u64, length: u64) -> Result<Option<bytes::Bytes>> {
        let entries = self.entries.lock().await;

        let end = offset + length;

        // The only candidate is the entry with the largest key <= offset.
        // range(..=offset).next_back() returns exactly that in O(log n).
        if let Some((&entry_offset, entry)) = entries.range(..=offset).next_back() {
            let entry_end = entry_offset + entry.data.len() as u64;
            if entry_end >= end {
                let start = (offset - entry_offset) as usize;
                let slice_end = start + length as usize;
                if slice_end <= entry.data.len() {
                    // Zero-copy slice
                    return Ok(Some(entry.data.slice(start..slice_end)));
                }
            }
        }

        Ok(None)
    }

    /// Flush all dirty entries, returning them for persistence.
    ///
    /// After flushing, entries remain in the cache but are marked as clean
    /// (eligible for future LRU eviction). The caller is responsible for
    /// writing the returned entries to durable storage.
    ///
    /// The returned CacheEntry clones are O(1): bytes::Bytes is an
    /// Arc-backed buffer, so cloning only bumps a reference count -- no
    /// data copy occurs.
    pub async fn flush(&self) -> Result<Vec<super::CacheEntry>> {
        let mut entries = self.entries.lock().await;

        let mut flushed = Vec::new();
        for entry in entries.values_mut() {
            if entry.dirty {
                // Clone is O(1): bytes::Bytes is Arc-backed (refcount bump only).
                flushed.push(entry.clone());
                entry.dirty = false;
            }
        }

        debug!("Flushed {} dirty cache entries", flushed.len());

        Ok(flushed)
    }
}
