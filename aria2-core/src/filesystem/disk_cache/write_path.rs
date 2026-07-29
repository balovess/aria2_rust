use std::sync::atomic::Ordering;
use tracing::debug;

use crate::error::Result;
use super::{CacheEntry, WrDiskCache};

/// Eviction target ratio: when over limit, evict down to this fraction of max size
const EVICTION_TARGET_RATIO: f64 = 0.5;

impl WrDiskCache {
    /// Write data at the given offset into the cache.
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
        let entry_size = data.len();
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);

        // Pre-check: if adding this entry will exceed the limit, try eviction first
        let current = self.total_cached_bytes.load(Ordering::Relaxed);
        if current.saturating_add(entry_size) > self.max_size_bytes {
            self.evict_clean_entries(entry_size).await;
        }

        let mut entries = self.entries.lock().await;

        // Re-check after acquiring lock (another task may have changed things)
        let current_locked = self.total_cached_bytes.load(Ordering::Relaxed);
        if current_locked.saturating_add(entry_size) > self.max_size_bytes {
            // Try again with lock held for precise accounting
            self.evict_clean_entries_locked(&mut entries, entry_size);
        }

        // Insert the new entry. If an entry already exists at this offset, it
        // is replaced -- the new write supersedes the old one. Subtract the old
        // entry's size so total_cached_bytes stays accurate.
        let old_size = entries
            .insert(
                offset,
                CacheEntry {
                    offset,
                    data,
                    dirty: true,
                    seq,
                },
            )
            .map(|old| old.size_bytes());
        if let Some(old) = old_size {
            self.total_cached_bytes.fetch_sub(old, Ordering::Relaxed);
        }
        self.total_cached_bytes
            .fetch_add(entry_size, Ordering::Relaxed);

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
    /// This method acquires the entries lock internally. For use when already holding
    /// the lock, see [evict_clean_entries_locked](Self::evict_clean_entries_locked).
    ///
    /// # Invariant
    /// Dirty entries are NEVER evicted. If all remaining entries are dirty and we still
    /// need space, the cache will temporarily exceed its limit rather than lose data.
    async fn evict_clean_entries(&self, needed_size: usize) {
        let mut entries = self.entries.lock().await;
        self.evict_clean_entries_locked(&mut entries, needed_size);
    }

    /// Core eviction logic -- must be called with entries lock held.
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
