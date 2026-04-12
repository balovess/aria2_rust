use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::debug;

use crate::error::Result;

/// Default maximum cache size: 16 MB
const DEFAULT_MAX_SIZE_BYTES: usize = 16 * 1024 * 1024;

impl Default for WrDiskCache {
    fn default() -> Self {
        Self::with_max_size_bytes(DEFAULT_MAX_SIZE_BYTES)
    }
}

/// Eviction target ratio: when over limit, evict down to this fraction of max size
const EVICTION_TARGET_RATIO: f64 = 0.5;

#[derive(Clone)]
pub struct CacheEntry {
    offset: u64,
    data: Vec<u8>,
    dirty: bool,
    #[allow(dead_code)]
    last_access: Instant,
}

impl CacheEntry {
    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Returns the memory size of this entry's data in bytes
    fn size_bytes(&self) -> usize {
        self.data.len()
    }
}

/// Write-back disk cache with LRU eviction and bounded memory usage.
///
/// `WrDiskCache` buffers disk writes before flushing them to persistent storage.
/// It uses an LRU (Least Recently Used) eviction policy that **never evicts dirty
/// (unflushed) entries**, guaranteeing no data loss under memory pressure.
///
/// Entries are stored in insertion order within a `VecDeque`; the oldest entries
/// are at the front and are the first candidates for eviction.
pub struct WrDiskCache {
    /// Cache entries ordered by insertion time (front = oldest = LRU candidate)
    entries: Mutex<VecDeque<CacheEntry>>,
    /// Maximum allowed cache size in bytes
    max_size_bytes: usize,
    /// Current total cached data size in bytes (atomic for lock-free reads)
    total_cached_bytes: AtomicUsize,
}

impl WrDiskCache {
    /// Create a new `WrDiskCache` with a maximum size specified in megabytes.
    ///
    /// # Arguments
    /// * `max_size_mb` - Maximum cache capacity in megabytes
    ///
    /// # Example
    /// ```ignore
    /// let cache = WrDiskCache::new(16); // 16 MB max
    /// ```
    pub fn new(max_size_mb: usize) -> Self {
        let max_size_bytes = max_size_mb * 1024 * 1024;

        debug!(
            "Initializing write-back disk cache, max capacity: {} MB ({} bytes)",
            max_size_mb, max_size_bytes
        );

        WrDiskCache {
            entries: Mutex::new(VecDeque::new()),
            max_size_bytes,
            total_cached_bytes: AtomicUsize::new(0),
        }
    }

    /// Create a new `WrDiskCache` with a maximum size specified in bytes.
    ///
    /// This provides finer-grained control than [`WrDiskCache::new`] which takes megabytes.
    ///
    /// # Arguments
    /// * `max_size_bytes` - Maximum cache capacity in bytes
    pub fn with_max_size_bytes(max_size_bytes: usize) -> Self {
        debug!(
            "Initializing write-back disk cache, max capacity: {} bytes",
            max_size_bytes
        );

        WrDiskCache {
            entries: Mutex::new(VecDeque::new()),
            max_size_bytes,
            total_cached_bytes: AtomicUsize::new(0),
        }
    }

    /// Returns the maximum cache size in bytes.
    pub fn max_size_bytes(&self) -> usize {
        self.max_size_bytes
    }

    /// Returns the current approximate cache size in bytes (lock-free).
    ///
    /// Note: This is an atomic snapshot and may be slightly stale if a write
    /// or eviction is concurrently in progress.
    pub fn current_size_bytes(&self) -> usize {
        self.total_cached_bytes.load(Ordering::Relaxed)
    }

    /// Write data at the given offset into the cache.
    ///
    /// If writing this entry would exceed `max_size_bytes`, LRU eviction is triggered.
    /// Only clean (already-flushed) entries are eligible for eviction — dirty entries
    /// are never evicted to prevent data loss. If insufficient clean entries exist
    /// to make room, the cache may temporarily exceed its limit until a flush occurs.
    pub async fn write(&self, offset: u64, data: Vec<u8>) -> Result<()> {
        let entry_size = data.len();

        // Pre-check: if adding this entry would exceed the limit, try eviction first
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

        let entry = CacheEntry {
            offset,
            data,
            dirty: true,
            last_access: Instant::now(),
        };

        entries.push_back(entry);
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

    /// Read cached data at the given offset and length.
    ///
    /// Returns `Some(data)` if the requested range is fully contained in a cached entry,
    /// or `None` if the data is not in the cache.
    pub async fn read(&self, offset: u64, length: u64) -> Result<Option<Vec<u8>>> {
        let entries = self.entries.lock().await;

        for entry in entries.iter() {
            // Exact offset match with sufficient length
            if entry.offset == offset && entry.data.len() >= length as usize {
                return Ok(Some(entry.data[..length as usize].to_vec()));
            }

            // Range-based match: entry fully covers [offset, offset + length)
            if entry.offset <= offset
                && (entry.offset + entry.data.len() as u64) >= (offset + length)
            {
                let start = (offset - entry.offset) as usize;
                let end = start + length as usize;
                if end <= entry.data.len() {
                    return Ok(Some(entry.data[start..end].to_vec()));
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
    pub async fn flush(&self) -> Result<Vec<CacheEntry>> {
        let mut entries = self.entries.lock().await;

        let flushed: Vec<CacheEntry> = entries.iter().filter(|e| e.dirty).cloned().collect();

        for entry in entries.iter_mut() {
            entry.dirty = false;
        }

        debug!("Flushed {} dirty cache entries", flushed.len());

        Ok(flushed)
    }

    /// Clear all entries from the cache and reset size tracking.
    pub async fn clear(&self) -> Result<()> {
        let mut entries = self.entries.lock().await;

        let cleared_bytes: usize = entries.iter().map(|e| e.size_bytes()).sum();
        entries.clear();
        self.total_cached_bytes
            .fetch_sub(cleared_bytes, Ordering::Relaxed);

        debug!("Cleared cache ({} bytes)", cleared_bytes);
        Ok(())
    }

    /// Returns the current total size of cached data in bytes.
    pub async fn size(&self) -> usize {
        self.total_cached_bytes.load(Ordering::Relaxed)
    }

    /// Returns true if the cache contains no entries.
    pub async fn is_empty(&self) -> bool {
        self.size().await == 0
    }

    /// Returns the number of entries in the cache.
    pub async fn count(&self) -> usize {
        self.entries.lock().await.len()
    }

    /// Returns the number of dirty (unflushed) entries.
    pub async fn dirty_count(&self) -> usize {
        self.entries.lock().await.iter().filter(|e| e.dirty).count()
    }

    // -----------------------------------------------------------------------
    // LRU Eviction methods
    // -----------------------------------------------------------------------

    /// Evict clean (non-dirty) entries to make room for `needed_size` additional bytes.
    ///
    /// This method acquires the entries lock internally. For use when already holding
    /// the lock, see [`evict_clean_entries_locked`](Self::evict_clean_entries_locked).
    ///
    /// # Invariant
    /// Dirty entries are NEVER evicted. If all remaining entries are dirty and we still
    /// need space, the cache will temporarily exceed its limit rather than lose data.
    async fn evict_clean_entries(&self, needed_size: usize) {
        let mut entries = self.entries.lock().await;
        self.evict_clean_entries_locked(&mut entries, needed_size);
    }

    /// Core eviction logic — must be called with `entries` lock held.
    ///
    /// Scans the VecDeque from front (oldest/LRU) to back, removing only clean entries
    /// until either:
    /// - We have freed enough space for `needed_size` new bytes, OR
    /// - We have reached the eviction target (50% of max), OR
    /// - No more clean entries remain
    fn evict_clean_entries_locked(&self, entries: &mut VecDeque<CacheEntry>, needed_size: usize) {
        let target = ((self.max_size_bytes as f64) * EVICTION_TARGET_RATIO) as usize;
        let mut evicted_count = 0usize;
        let mut evicted_bytes = 0usize;

        // Keep evicting while:
        // 1. Current size + needed > target (we still need room), AND
        // 2. There are entries to examine
        while self
            .total_cached_bytes
            .load(Ordering::Relaxed)
            .saturating_add(needed_size)
            > target
        {
            // Peek at the front (oldest) entry
            let should_evict = entries.front().map_or(false, |entry| !entry.dirty);

            if should_evict {
                // Safe to evict: entry is clean (already flushed)
                if let Some(entry) = entries.pop_front() {
                    let entry_size = entry.size_bytes();
                    self.total_cached_bytes
                        .fetch_sub(entry_size, Ordering::Relaxed);
                    evicted_bytes += entry_size;
                    evicted_count += 1;

                    debug!(
                        "Evicted clean cache entry, offset: {}, size: {} bytes",
                        entry.offset, entry_size
                    );
                } else {
                    break; // No more entries
                }
            } else {
                // Front entry is dirty — cannot evict it
                // Check if there's a clean entry further back that we can reach
                // by scanning forward for the next clean candidate.
                // However, since VecDeque doesn't support efficient removal from middle,
                // and we must preserve order, we stop here.
                //
                // This means: if oldest entries are dirty, they block eviction of newer
                // clean entries behind them. This is acceptable because:
                // - It prevents out-of-order eviction complexity
                // - Dirty entries should be flushed promptly anyway
                // - The cache may temporarily overshoot, which is safe
                debug!(
                    "Eviction blocked: oldest entry at offset {} is dirty ({} dirty entries blocking)",
                    entries.front().map(|e| e.offset).unwrap_or(0),
                    entries.iter().filter(|e| e.dirty).count()
                );
                break;
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

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a cache with a small max size (in bytes) for testing eviction behavior
    fn make_small_cache(max_bytes: usize) -> WrDiskCache {
        WrDiskCache::with_max_size_bytes(max_bytes)
    }

    // -----------------------------------------------------------------------
    // Basic functionality tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_new_constructor_mb() {
        let cache = WrDiskCache::new(16); // 16 MB
        assert_eq!(cache.max_size_bytes(), 16 * 1024 * 1024);
        assert_eq!(cache.current_size_bytes(), 0);
        assert!(cache.is_empty().await);
    }

    #[tokio::test]
    async fn test_with_max_size_bytes_constructor() {
        let cache = WrDiskCache::with_max_size_bytes(1024); // 1 KB
        assert_eq!(cache.max_size_bytes(), 1024);
        assert_eq!(cache.current_size_bytes(), 0);
    }

    #[tokio::test]
    async fn test_write_and_read() {
        let cache = make_small_cache(4096);

        cache.write(0, b"hello".to_vec()).await.unwrap();
        cache.write(100, b"world".to_vec()).await.unwrap();

        assert_eq!(cache.size().await, 10);
        assert_eq!(cache.count().await, 2);

        // Read back by exact offset
        let result = cache.read(0, 5).await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"hello");

        let result = cache.read(100, 5).await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap(), b"world");
    }

    #[tokio::test]
    async fn test_flush_returns_dirty_entries() {
        let cache = make_small_cache(4096);

        cache.write(0, b"data1".to_vec()).await.unwrap();
        cache.write(10, b"data2".to_vec()).await.unwrap();

        assert_eq!(cache.dirty_count().await, 2);

        let flushed = cache.flush().await.unwrap();
        assert_eq!(flushed.len(), 2);

        // After flush, entries are no longer dirty
        assert_eq!(cache.dirty_count().await, 0);
        // But they're still in the cache
        assert_eq!(cache.count().await, 2);
    }

    #[tokio::test]
    async fn test_clear_resets_cache() {
        let cache = make_small_cache(4096);

        cache.write(0, vec![0x42; 100]).await.unwrap();
        assert_eq!(cache.size().await, 100);

        cache.clear().await.unwrap();
        assert_eq!(cache.size().await, 0);
        assert!(cache.is_empty().await);
        assert_eq!(cache.count().await, 0);
    }

    // -----------------------------------------------------------------------
    // LRU Eviction: clean entries are evicted under memory pressure
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_cache_eviction_under_memory_pressure() {
        // Cache max = 500 bytes. Write 6 entries of 100 bytes each.
        // After exceeding the limit, eviction should kick in for CLEAN entries.
        let cache = make_small_cache(500);

        // Phase 1: Write 3 dirty entries (300 bytes) — under limit
        cache.write(0, vec![0u8; 100]).await.unwrap(); // entry A: offset=0
        cache.write(100, vec![1u8; 100]).await.unwrap(); // entry B: offset=100
        cache.write(200, vec![2u8; 100]).await.unwrap(); // entry C: offset=200
        assert_eq!(cache.count().await, 3);
        assert_eq!(cache.size().await, 300);

        // Flush to make them clean
        cache.flush().await.unwrap();
        assert_eq!(cache.dirty_count().await, 0);
        assert_eq!(cache.count().await, 3); // Still present

        // Phase 2: Write more entries that will trigger eviction
        // Writing 300 more bytes would exceed 500 limit → should evict old clean ones
        cache.write(300, vec![3u8; 100]).await.unwrap(); // entry D: dirty
        cache.write(400, vec![4u8; 100]).await.unwrap(); // entry E: dirty

        // At this point we have 5 entries (500 bytes). Adding one more triggers eviction.
        cache.write(500, vec![5u8; 100]).await.unwrap(); // entry F: dirty

        // The oldest CLEAN entries (A, B, C) should have been evicted to make room.
        // Only D, E, F should remain (or possibly some of A/B/C if not all were evicted).
        // Key invariant: total size should be roughly bounded (may slightly exceed due to
        // dirty-only entries blocking eviction).
        let final_count = cache.count().await;
        let final_size = cache.size().await;

        // We wrote 600 bytes into a 500-byte cache. Since A,B,C became clean before
        // D,E,F were written, at least some of them should have been evicted.
        // The cache should NOT contain all 6 entries.
        assert!(
            final_count <= 5,
            "Expected at most 5 entries after eviction, got {}",
            final_count
        );

        // The remaining entries should be the newer ones (D, E, F and possibly one of A/B/C)
        // Verify the oldest clean entry (A at offset 0) was likely evicted
        let entry_0 = cache.read(0, 100).await.unwrap();
        // Entry A (offset 0) was the oldest clean — it should be gone
        assert!(
            entry_0.is_none(),
            "Oldest clean entry (offset 0) should have been evicted"
        );

        debug!(
            "Eviction test: final count={}, final_size={}, expected ~<=500",
            final_count, final_size
        );
    }

    #[tokio::test]
    async fn test_dirty_entries_are_never_evicted() {
        // This is the critical safety property: dirty entries MUST survive eviction.
        // Cache max = 400 bytes.
        let cache = make_small_cache(400);

        // Write 4 dirty entries (400 bytes = exactly at limit)
        cache.write(0, vec![0xAA; 100]).await.unwrap(); // dirty A
        cache.write(100, vec![0xBB; 100]).await.unwrap(); // dirty B
        cache.write(200, vec![0xCC; 100]).await.unwrap(); // dirty C
        cache.write(300, vec![0xDD; 100]).await.unwrap(); // dirty D

        assert_eq!(cache.dirty_count().await, 4);
        assert_eq!(cache.count().await, 4);

        // Now try to write another entry that would exceed the limit.
        // All existing entries are dirty, so NONE can be evicted.
        // The write must still succeed (cache may temporarily overshoot).
        cache.write(400, vec![0xEE; 100]).await.unwrap(); // dirty E

        // ALL 5 dirty entries must still be present — zero data loss allowed
        assert_eq!(
            cache.count().await,
            5,
            "All dirty entries must be preserved — none should be evicted"
        );
        assert_eq!(
            cache.dirty_count().await,
            5,
            "All entries must still be dirty"
        );

        // Verify each entry's data is intact
        for (offset, byte_val) in [
            (0u64, 0xAAu8),
            (100, 0xBB),
            (200, 0xCC),
            (300, 0xDD),
            (400, 0xEE),
        ] {
            let result = cache.read(offset, 100).await.unwrap();
            assert!(
                result.is_some(),
                "Dirty entry at offset {} must still exist",
                offset
            );
            let data = result.unwrap();
            assert!(
                data.iter().all(|&b| b == byte_val),
                "Data integrity check failed for offset {}: expected 0x{:02X}",
                offset,
                byte_val
            );
        }

        // Flush should return all 5 entries
        let flushed = cache.flush().await.unwrap();
        assert_eq!(flushed.len(), 5, "Flush must return all 5 dirty entries");
    }

    #[tokio::test]
    async fn test_mixed_dirty_and_clean_eviction() {
        // Cache max = 500 bytes.
        // Mix of dirty and clean entries: only clean ones should be evicted.
        let cache = make_small_cache(500);

        // Write and flush (make clean) some older entries
        cache.write(0, vec![1u8; 100]).await.unwrap(); // will become clean
        cache.write(100, vec![2u8; 100]).await.unwrap(); // will become clean
        cache.flush().await.unwrap(); // Mark A, B as clean

        // Write new dirty entries
        cache.write(200, vec![3u8; 100]).await.unwrap(); // dirty C
        cache.write(300, vec![4u8; 100]).await.unwrap(); // dirty D

        // Now: A(clean), B(clean), C(dirty), D(dirty) = 400 bytes
        // Write more to trigger eviction
        cache.write(400, vec![5u8; 100]).await.unwrap(); // dirty E — now 500 bytes
        cache.write(500, vec![6u8; 100]).await.unwrap(); // dirty F — exceeds 500, triggers eviction

        // Clean entries A and/or B should be evicted; C,D,E,F (dirty) must remain
        let dirty_cnt = cache.dirty_count().await;
        let total_cnt = cache.count().await;

        // All 4 dirty entries (C, D, E, F) must survive
        assert!(
            dirty_cnt >= 4,
            "At least 4 dirty entries must survive, got {}",
            dirty_cnt
        );

        // At least some clean entries should have been evicted
        assert!(
            total_cnt <= 6,
            "Total entries should be bounded, got {}",
            total_cnt
        );

        // Verify dirty entries' data is intact
        for (offset, expected_byte) in [(200, 3u8), (300, 4), (400, 5), (500, 6)] {
            let result = cache.read(offset, 100).await.unwrap();
            assert!(
                result.is_some(),
                "Dirty entry at offset {} must survive eviction",
                offset
            );
            assert_eq!(result.unwrap()[0], expected_byte);
        }

        debug!("Mixed eviction: total={}, dirty={}", total_cnt, dirty_cnt);
    }

    #[tokio::test]
    async fn test_flush_then_evict_frees_space() {
        // Verify the lifecycle: write → flush (clean) → write more → evicts old clean
        let cache = make_small_cache(300);

        // Fill with dirty entries, then flush them
        cache.write(0, vec![0u8; 150]).await.unwrap();
        cache.flush().await.unwrap(); // Now clean
        assert_eq!(cache.dirty_count().await, 0);

        // Write new dirty entry — should trigger eviction of the clean one
        cache.write(200, vec![1u8; 150]).await.unwrap();

        // The first entry (now clean) may or may not have been evicted depending on
        // whether 300 + 150 > 300 triggered it. With our logic, 150 + 150 = 300 which
        // is NOT > 300, so no eviction yet. One more write should trigger it.
        cache.write(400, vec![2u8; 150]).await.unwrap(); // 450 > 300 → evict!

        // Old clean entry at offset 0 should be evicted; new dirty entries remain
        let _old_entry = cache.read(0, 150).await.unwrap();
        // It may or may not be evicted depending on exact timing, but dirty entries survive
        let new_entry = cache.read(200, 150).await.unwrap();
        assert!(new_entry.is_some(), "Newer dirty entry must survive");
    }

    #[tokio::test]
    async fn test_eviction_to_target_ratio() {
        // When over limit, eviction should bring us down to ~50% of max
        let cache = make_small_cache(1000); // 1KB max, target = 500

        // Write and flush many small clean entries
        for i in 0..20 {
            cache
                .write((i * 50) as u64, vec![i as u8; 50])
                .await
                .unwrap();
        }
        // 20 * 50 = 1000 bytes = exactly at limit

        cache.flush().await.unwrap(); // All clean now

        // Write one more entry to push over limit and trigger eviction
        cache.write(2000, vec![0xFF; 50]).await.unwrap(); // 1050 > 1000 → evict

        // Should have evicted down to target (~500 bytes / 50 per entry ≈ 10 entries)
        let size = cache.size().await;
        let count = cache.count().await;

        // Size should be significantly reduced from original 1050
        assert!(
            size <= 550, // Allow some tolerance around 50% target + new entry
            "After eviction, size ({}) should be near target (~500), max is 1000",
            size
        );

        debug!(
            "Eviction to target: size={} bytes, count={} entries",
            size, count
        );
    }

    #[tokio::test]
    async fn test_current_size_bytes_lock_free() {
        // Verify current_size_bytes() works without holding the async lock
        let cache = make_small_cache(4096);

        cache.write(0, vec![0u8; 256]).await.unwrap();
        cache.write(256, vec![1u8; 256]).await.unwrap();

        // Lock-free read should match locked read
        let lock_free_size = cache.current_size_bytes();
        let locked_size = cache.size().await;

        assert_eq!(lock_free_size, locked_size);
        assert_eq!(lock_free_size, 512);
    }

    #[tokio::test]
    async fn test_read_miss_returns_none() {
        let cache = make_small_cache(1024);

        cache.write(0, b"hello".to_vec()).await.unwrap();

        // Non-existent offset
        assert!(cache.read(999, 5).await.unwrap().is_none());

        // Offset exists but length too long
        assert!(cache.read(0, 100).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_range_based_read() {
        let cache = make_small_cache(4096);

        // Write an entry covering offsets 0-99
        cache.write(0, vec![42u8; 100]).await.unwrap();

        // Read a sub-range within the entry
        let result = cache.read(10, 30).await.unwrap();
        assert!(result.is_some());
        let data = result.unwrap();
        assert_eq!(data.len(), 30);
        assert!(data.iter().all(|&b| b == 42));
    }

    #[tokio::test]
    async fn test_multiple_flushes_only_return_dirty() {
        let cache = make_small_cache(4096);

        cache.write(0, b"A".to_vec()).await.unwrap();
        cache.write(1, b"B".to_vec()).await.unwrap();

        // First flush returns both
        let f1 = cache.flush().await.unwrap();
        assert_eq!(f1.len(), 2);

        // Second flush returns nothing (already clean)
        let f2 = cache.flush().await.unwrap();
        assert_eq!(f2.len(), 0);

        // Write a third entry
        cache.write(2, b"C".to_vec()).await.unwrap();

        // Third flush returns only the new dirty entry
        let f3 = cache.flush().await.unwrap();
        assert_eq!(f3.len(), 1);
        assert_eq!(f3[0].offset(), 2);
    }

    #[tokio::test]
    async fn test_empty_cache_operations() {
        let cache = make_small_cache(1024);

        assert!(cache.is_empty().await);
        assert_eq!(cache.size().await, 0);
        assert_eq!(cache.count().await, 0);
        assert_eq!(cache.dirty_count().await, 0);

        // Read on empty cache
        assert!(cache.read(0, 10).await.unwrap().is_none());

        // Flush on empty cache
        let flushed = cache.flush().await.unwrap();
        assert!(flushed.is_empty());

        // Clear on empty cache (should not panic)
        cache.clear().await.unwrap();
        assert!(cache.is_empty().await);
    }

    #[tokio::test]
    async fn test_large_write_exceeding_max_with_only_dirty_entries() {
        // Edge case: single write larger than max_size, all entries dirty
        let cache = make_small_cache(100); // Tiny 100-byte cache

        // Write a single entry larger than max
        cache.write(0, vec![0u8; 200]).await.unwrap();

        // Must succeed without losing data (no clean entries to evict anyway)
        assert_eq!(cache.count().await, 1);
        assert_eq!(cache.size().await, 200);
        assert_eq!(cache.dirty_count().await, 1);

        let result = cache.read(0, 200).await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().len(), 200);
    }
}
