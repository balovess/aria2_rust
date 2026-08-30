use tracing::debug;

use super::WrDiskCache;
use crate::error::Result;

impl WrDiskCache {
    /// Read cached data at the given offset and length.
    ///
    /// Returns Some(data) if the requested range is fully contained in a cached entry,
    /// or None if the data is not in the cache.
    ///
    /// # Complexity
    /// O(log n + k), where `k` is the number of adjacent entries needed to
    /// cover the requested range. Entries are kept disjoint by `write()`;
    /// single-entry reads stay zero-copy while a range spanning fragments is
    /// assembled into one contiguous `Bytes` value.
    ///
    /// # Zero-Copy
    /// Returns bytes::Bytes slice instead of Vec<u8>, avoiding memory allocation.
    pub async fn read(&self, offset: u64, length: u64) -> Result<Option<bytes::Bytes>> {
        let entries = self.entries.lock().await;

        let end = offset.checked_add(length).ok_or_else(|| {
            crate::error::Aria2Error::InvalidArgument(
                "read range exceeds u64 address space".to_string(),
            )
        })?;
        let length = usize::try_from(length).map_err(|_| {
            crate::error::Aria2Error::InvalidArgument(
                "read range does not fit in platform address space".to_string(),
            )
        })?;

        // Start at the entry containing (or immediately before) the request.
        let Some((&entry_offset, entry)) = entries.range(..=offset).next_back() else {
            self.cache_misses
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(None);
        };
        let entry_end = entry_offset
            .checked_add(entry.data.len() as u64)
            .ok_or_else(|| {
                crate::error::Aria2Error::InvalidArgument(
                    "cached entry exceeds u64 address space".to_string(),
                )
            })?;
        if entry_end >= end {
            let start = (offset - entry_offset) as usize;
            let slice_end = start.checked_add(length).ok_or_else(|| {
                crate::error::Aria2Error::InvalidArgument(
                    "read range does not fit in cached entry".to_string(),
                )
            })?;
            self.cache_hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(Some(entry.data.slice(start..slice_end)));
        }

        // The request spans multiple adjacent fragments. Build the result in
        // one allocation, but reject the range as soon as a gap is observed.
        let mut result = Vec::with_capacity(length);
        let mut cursor = offset;
        for (&fragment_offset, fragment) in entries.range(entry_offset..) {
            let fragment_end = fragment_offset
                .checked_add(fragment.data.len() as u64)
                .ok_or_else(|| {
                    crate::error::Aria2Error::InvalidArgument(
                        "cached entry exceeds u64 address space".to_string(),
                    )
                })?;
            if fragment_end <= cursor {
                continue;
            }
            if fragment_offset > cursor {
                self.cache_misses
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Ok(None);
            }

            let copy_start = (cursor - fragment_offset) as usize;
            let copy_end = ((end.min(fragment_end)) - fragment_offset) as usize;
            result.extend_from_slice(&fragment.data[copy_start..copy_end]);
            cursor = end.min(fragment_end);
            if cursor == end {
                self.cache_hits
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Ok(Some(bytes::Bytes::from(result)));
            }
        }

        self.cache_misses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        let _flush_guard = self.flush_gate.lock().await;
        let mut entries = self.entries.lock().await;

        let mut flushed = Vec::new();
        for entry in entries.values_mut() {
            if entry.dirty {
                // Clone is O(1): bytes::Bytes is Arc-backed (refcount bump only).
                flushed.push(entry.clone());
                entry.dirty = false;
                self.enqueue_clean(entry.offset, entry.seq);
            }
        }

        debug!("Flushed {} dirty cache entries", flushed.len());

        Ok(flushed)
    }
}
