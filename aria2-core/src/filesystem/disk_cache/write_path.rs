use std::sync::atomic::Ordering;
use tracing::debug;

use super::{CacheEntry, WrDiskCache};
use crate::error::Result;

/// Eviction target ratio: when over limit, evict down to this fraction of max size
const EVICTION_TARGET_RATIO: f64 = 0.5;

impl WrDiskCache {
    /// Write data at the given offset into the cache.
    ///
    /// The cache maintains non-overlapping entries. A new write supersedes
    /// the covered portion of older entries while preserving their untouched
    /// left and right fragments. This makes range reads and flushes obey the
    /// same last-write-wins ordering as the eventual file contents.
    ///
    /// If writing this entry will exceed max_size_bytes, LRU eviction is triggered.
    /// Only clean (already-flushed) entries are eligible for eviction -- dirty entries
    /// are never evicted to prevent data loss. If insufficient clean entries exist
    /// to make room, the cache may temporarily exceed its limit until a flush occurs.
    ///
    /// # Zero-Copy
    /// This method accepts bytes::Bytes which enables zero-copy slicing.
    /// The caller can pass a slice of a larger buffer without copying.
    pub async fn write(&self, offset: u64, data: bytes::Bytes) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        let entry_size = data.len();
        let end = offset.checked_add(entry_size as u64).ok_or_else(|| {
            crate::error::Aria2Error::InvalidArgument(
                "write range exceeds u64 address space".to_string(),
            )
        })?;
        let _flush_guard = self.flush_gate.lock().await;
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let mut entries = self.entries.lock().await;

        // Split every entry touched by the new range into the portions that
        // remain visible. The map invariant after this block is that entries
        // are pairwise disjoint, so one predecessor lookup is sufficient for
        // a complete range read.
        // Cache entries are kept pairwise disjoint. Only the predecessor of
        // `offset` can overlap from the left; subsequent overlaps must start
        // inside the new range. Avoid scanning every older entry here: that
        // turns sequential small writes into an O(n^2) workload.
        let mut overlapping_keys = Vec::new();
        if let Some((&key, entry)) = entries.range(..=offset).next_back()
            && entry
                .offset
                .checked_add(entry.data.len() as u64)
                .is_some_and(|entry_end| entry_end > offset)
        {
            overlapping_keys.push(key);
        }
        overlapping_keys.extend(
            entries
                .range((
                    std::ops::Bound::Excluded(offset),
                    std::ops::Bound::Unbounded,
                ))
                .take_while(|(key, _)| **key < end)
                .map(|(&key, _)| key),
        );

        let mut retained = Vec::with_capacity(overlapping_keys.len() * 2);
        for key in overlapping_keys {
            let entry = entries
                .remove(&key)
                .expect("overlapping cache entry must still exist");
            self.total_cached_bytes
                .fetch_sub(entry.size_bytes(), Ordering::Relaxed);

            let entry_end = entry.offset + entry.data.len() as u64;
            if entry.offset < offset {
                let left_len = (offset - entry.offset) as usize;
                retained.push(CacheEntry {
                    offset: entry.offset,
                    data: entry.data.slice(..left_len),
                    dirty: entry.dirty,
                    seq: entry.seq,
                });
            }
            if entry_end > end {
                let right_start = (end - entry.offset) as usize;
                retained.push(CacheEntry {
                    offset: end,
                    data: entry.data.slice(right_start..),
                    dirty: entry.dirty,
                    seq: entry.seq,
                });
            }
        }

        for entry in retained {
            self.total_cached_bytes
                .fetch_add(entry.size_bytes(), Ordering::Relaxed);
            entries.insert(entry.offset, entry);
        }

        entries.insert(
            offset,
            CacheEntry {
                offset,
                data,
                dirty: true,
                seq,
            },
        );
        self.total_cached_bytes
            .fetch_add(entry_size, Ordering::Relaxed);

        // Normalize before eviction so the size accounting is based on the
        // actual disjoint representation.
        if self.total_cached_bytes.load(Ordering::Relaxed) > self.max_size_bytes {
            self.evict_clean_entries_locked(&mut entries, 0);
        }

        debug!(
            "Wrote to cache, offset: {}, size: {}, cache usage: {}/{} bytes",
            offset,
            entry_size,
            self.total_cached_bytes.load(Ordering::Relaxed),
            self.max_size_bytes
        );

        Ok(())
    }

    // -----------------------------------------------------------------------
    // LRU Eviction methods
    // -----------------------------------------------------------------------

    /// Evict clean (non-dirty) entries to make room for `needed_size` additional bytes.
    ///
    /// Core eviction logic -- must be called with both `flush_gate` and
    /// `entries` held.
    ///
    /// Repeatedly removes the clean (non-dirty) entry with the smallest seq
    /// (oldest insertion = LRU candidate) until either:
    /// - We have freed enough space for `needed_size` new bytes, OR
    /// - We have reached the eviction target (50% of max), OR
    /// - No more clean entries remain
    ///
    /// Unlike the old VecDeque front-only eviction, the BTreeMap + seq
    /// design can evict ANY clean entry regardless of its offset ordering --
    /// a dirty entry no longer blocks eviction of newer clean entries behind it.
    ///
    /// # Complexity
    /// Each iteration is O(n) (a full scan to find the min-seq clean entry),
    /// but eviction is infrequent (only when over the memory limit), so this is
    /// acceptable. The common read/write paths remain O(log n).
    pub(crate) fn evict_clean_entries_locked(
        &self,
        entries: &mut std::collections::BTreeMap<u64, CacheEntry>,
        needed_size: usize,
    ) {
        let target = ((self.max_size_bytes as f64) * EVICTION_TARGET_RATIO) as usize;
        let mut evicted_count = 0usize;
        let mut evicted_bytes = 0usize;

        // Keep evicting while current size + needed > target (we still need room)
        while self
            .total_cached_bytes
            .load(Ordering::Relaxed)
            .saturating_add(needed_size)
            > target
        {
            // Find the clean entry with the smallest seq (true LRU order).
            // Iterating all entries is O(n), but eviction is rare.
            let evict_key = entries
                .iter()
                .filter(|(_, e)| !e.dirty)
                .min_by_key(|(_, e)| e.seq)
                .map(|(&k, _)| k);

            match evict_key {
                Some(key) => {
                    if let Some(entry) = entries.remove(&key) {
                        let entry_size = entry.size_bytes();
                        self.total_cached_bytes
                            .fetch_sub(entry_size, Ordering::Relaxed);
                        evicted_bytes += entry_size;
                        evicted_count += 1;

                        debug!(
                            "Evicted clean cache entry (seq {}), offset: {}, size: {} bytes",
                            entry.seq, entry.offset, entry_size
                        );
                    }
                }
                None => {
                    // No clean entries remain -- cannot evict without losing data.
                    // The cache may temporarily overshoot, which is safe.
                    debug!(
                        "Eviction blocked: all {} remaining entries are dirty",
                        entries.len()
                    );
                    break;
                }
            }
        }

        if evicted_count > 0 {
            debug!(
                "LRU eviction complete: evicted {} entries ({} bytes), cache now ~{} bytes",
                evicted_count,
                evicted_bytes,
                self.total_cached_bytes.load(Ordering::Relaxed)
            );
        }
    }
}
