use bytes::{Bytes, BytesMut};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::sync::Mutex;
use tracing::debug;

use crate::error::Result;

/// Default maximum cache size: 16 MB
const DEFAULT_MAX_SIZE_BYTES: usize = 16 * 1024 * 1024;
const MAX_COALESCED_FLUSH_BYTES: usize = 1024 * 1024;

enum CoalescedData {
    Shared(Bytes),
    Merged(BytesMut),
}

/// Runtime counters for observing write-back cache behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskCacheStats {
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub flush_count: u64,
    pub flush_pending_write_count: u64,
    pub flush_write_count: u64,
    pub flush_write_bytes: u64,
    pub clean_eviction_count: u64,
    pub dirty_eviction_count: u64,
}

fn coalesce_flush_entries(pending: &[(u64, Bytes, u64)]) -> Vec<(u64, Bytes)> {
    let mut coalesced: Vec<(u64, CoalescedData)> = Vec::new();
    for (offset, data, _) in pending {
        let can_extend = coalesced.last().is_some_and(|(start, current)| {
            let current_len = match current {
                CoalescedData::Shared(current) => current.len(),
                CoalescedData::Merged(current) => current.len(),
            };
            start
                .checked_add(current_len as u64)
                .is_some_and(|end| end == *offset)
                && current_len + data.len() <= MAX_COALESCED_FLUSH_BYTES
        });

        if can_extend {
            let (start, current) = coalesced
                .pop()
                .expect("coalesced entry exists when extending");
            let mut merged = match current {
                CoalescedData::Shared(current) => {
                    // Reserve geometrically so a chain of adjacent chunks is
                    // copied once into a growing buffer instead of once per
                    // merge (which would make the flush O(n^2)).
                    let required = current.len() + data.len();
                    let capacity = required.next_power_of_two().min(MAX_COALESCED_FLUSH_BYTES);
                    let mut merged = BytesMut::with_capacity(capacity);
                    merged.extend_from_slice(&current);
                    merged
                }
                CoalescedData::Merged(merged) => merged,
            };
            merged.extend_from_slice(data);
            coalesced.push((start, CoalescedData::Merged(merged)));
        } else {
            // Cloning Bytes only increments its reference count. Keep the
            // original allocation when this range does not need merging.
            coalesced.push((*offset, CoalescedData::Shared(data.clone())));
        }
    }
    coalesced
        .into_iter()
        .map(|(offset, data)| {
            let data = match data {
                CoalescedData::Shared(data) => data,
                CoalescedData::Merged(data) => data.freeze(),
            };
            (offset, data)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Sub-modules implementing write-path and read-path operations
// ---------------------------------------------------------------------------

pub(crate) mod read_path;
pub(crate) mod write_path;

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// CacheEntry
// ---------------------------------------------------------------------------

/// A single cached data region.
///
/// Each entry maps a contiguous byte range starting at offset with length
/// data.len(). The dirty flag tracks whether the entry has been flushed
/// to persistent storage. The seq field provides monotonic LRU ordering.
#[derive(Clone)]
pub struct CacheEntry {
    offset: u64,
    data: bytes::Bytes, // Zero-copy immutable buffer
    dirty: bool,
    /// Monotonic insertion sequence number used for LRU eviction ordering.
    /// Lower seq = older insertion = evicted first.
    seq: u64,
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

    /// Consume the entry and return the underlying Bytes buffer.
    ///
    /// This enables zero-copy transfer of cached data to the disk writer
    /// without cloning the buffer. The entry's Bytes is an Arc-backed
    /// buffer, so moving it out is O(1) -- no data copy occurs.
    pub fn into_data(self) -> bytes::Bytes {
        self.data
    }

    /// Returns the memory size of this entry's data in bytes.
    pub(crate) fn size_bytes(&self) -> usize {
        self.data.len()
    }
}

// ---------------------------------------------------------------------------
// WrDiskCache
// ---------------------------------------------------------------------------

/// Write-back disk cache with LRU eviction and bounded memory usage.
///
/// WrDiskCache buffers disk writes before flushing them to persistent storage.
/// It uses an LRU (Least Recently Used) eviction policy that **never evicts dirty
/// (unflushed) entries**, guaranteeing no data loss under memory pressure.
///
/// Entries are keyed by their start offset in a [BTreeMap], enabling O(log n)
/// range lookups for read(). LRU ordering is preserved via a per-entry monotonic
/// seq number -- during eviction the clean entry with the smallest seq (oldest
/// insertion) is removed first.
pub struct WrDiskCache {
    /// Cache entries keyed by start offset, enabling O(log n) range queries.
    pub(crate) entries: Mutex<BTreeMap<u64, CacheEntry>>,
    /// Lazy LRU index for clean entries. Stale nodes are discarded during
    /// eviction after checking the current map entry and sequence number.
    clean_lru: StdMutex<BinaryHeap<Reverse<(u64, u64)>>>,
    /// Serializes cache mutations with flushes that perform external I/O.
    ///
    /// The entry lock is intentionally released while a caller-provided
    /// writer is awaited. This gate prevents a concurrent write from being
    /// overwritten by a stale flush snapshot without holding the map lock
    /// across disk I/O.
    pub(crate) flush_gate: Mutex<()>,
    /// Maximum allowed cache size in bytes
    pub(crate) max_size_bytes: usize,
    /// Current total cached data size in bytes (atomic for lock-free reads)
    pub(crate) total_cached_bytes: AtomicUsize,
    /// Monotonic counter assigning insertion sequence numbers for LRU ordering.
    pub(crate) next_seq: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    flush_count: AtomicU64,
    flush_pending_write_count: AtomicU64,
    flush_write_count: AtomicU64,
    flush_write_bytes: AtomicU64,
    clean_eviction_count: AtomicU64,
    dirty_eviction_count: AtomicU64,
}

impl Default for WrDiskCache {
    fn default() -> Self {
        Self::with_max_size_bytes(DEFAULT_MAX_SIZE_BYTES)
    }
}

impl WrDiskCache {
    /// Create a new WrDiskCache with a maximum size specified in megabytes.
    ///
    /// # Arguments
    /// * max_size_mb - Maximum cache capacity in megabytes
    ///
    /// # Example
    /// `ignore
    /// let cache = WrDiskCache::new(16); // 16 MB max
    /// `
    pub fn new(max_size_mb: usize) -> Self {
        let max_size_bytes = max_size_mb.saturating_mul(1024 * 1024);

        debug!(
            "Initializing write-back disk cache, max capacity: {} MB ({} bytes)",
            max_size_mb, max_size_bytes
        );

        WrDiskCache {
            entries: Mutex::new(BTreeMap::new()),
            clean_lru: StdMutex::new(BinaryHeap::new()),
            flush_gate: Mutex::new(()),
            max_size_bytes,
            total_cached_bytes: AtomicUsize::new(0),
            next_seq: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            flush_count: AtomicU64::new(0),
            flush_pending_write_count: AtomicU64::new(0),
            flush_write_count: AtomicU64::new(0),
            flush_write_bytes: AtomicU64::new(0),
            clean_eviction_count: AtomicU64::new(0),
            dirty_eviction_count: AtomicU64::new(0),
        }
    }

    /// Create a new WrDiskCache with a maximum size specified in bytes.
    ///
    /// This provides finer-grained control than [WrDiskCache::new] which takes megabytes.
    ///
    /// # Arguments
    /// * max_size_bytes - Maximum cache capacity in bytes
    pub fn with_max_size_bytes(max_size_bytes: usize) -> Self {
        debug!(
            "Initializing write-back disk cache, max capacity: {} bytes",
            max_size_bytes
        );

        WrDiskCache {
            entries: Mutex::new(BTreeMap::new()),
            clean_lru: StdMutex::new(BinaryHeap::new()),
            flush_gate: Mutex::new(()),
            max_size_bytes,
            total_cached_bytes: AtomicUsize::new(0),
            next_seq: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            flush_count: AtomicU64::new(0),
            flush_pending_write_count: AtomicU64::new(0),
            flush_write_count: AtomicU64::new(0),
            flush_write_bytes: AtomicU64::new(0),
            clean_eviction_count: AtomicU64::new(0),
            dirty_eviction_count: AtomicU64::new(0),
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
        self.entries
            .lock()
            .await
            .values()
            .filter(|e| e.dirty)
            .count()
    }

    /// Return a point-in-time snapshot of cache and write-back counters.
    pub fn stats(&self) -> DiskCacheStats {
        DiskCacheStats {
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
            flush_count: self.flush_count.load(Ordering::Relaxed),
            flush_pending_write_count: self.flush_pending_write_count.load(Ordering::Relaxed),
            flush_write_count: self.flush_write_count.load(Ordering::Relaxed),
            flush_write_bytes: self.flush_write_bytes.load(Ordering::Relaxed),
            clean_eviction_count: self.clean_eviction_count.load(Ordering::Relaxed),
            dirty_eviction_count: self.dirty_eviction_count.load(Ordering::Relaxed),
        }
    }

    /// Flush dirty entries through a caller-provided positioned writer.
    ///
    /// Entries are marked clean only after the writer reports success, so a
    /// failed disk write remains retryable instead of being silently lost.
    pub async fn flush_to(
        &self,
        writer: &mut dyn crate::filesystem::disk_writer::SeekableDiskWriter,
    ) -> Result<()> {
        let _flush_guard = self.flush_gate.lock().await;
        let pending: Vec<_> = {
            let entries = self.entries.lock().await;
            entries
                .values()
                .filter(|entry| entry.dirty)
                .map(|entry| (entry.offset, entry.data.clone(), entry.seq))
                .collect()
        };

        // Network chunks are commonly adjacent but much smaller than the
        // blocking-pool and syscall overhead. Coalesce only contiguous ranges
        // and cap the merged buffer so sparse/out-of-order writes retain their
        // original semantics and memory remains bounded.
        self.flush_pending_write_count
            .fetch_add(pending.len() as u64, Ordering::Relaxed);
        for (offset, data) in coalesce_flush_entries(&pending) {
            let data_len = data.len() as u64;
            writer.write_bytes_at(offset, data).await?;
            self.flush_write_count.fetch_add(1, Ordering::Relaxed);
            self.flush_write_bytes
                .fetch_add(data_len, Ordering::Relaxed);
        }
        writer.flush().await?;
        self.flush_count.fetch_add(1, Ordering::Relaxed);

        let mut entries = self.entries.lock().await;
        for (offset, _, seq) in pending {
            if let Some(entry) = entries.get_mut(&offset)
                && entry.seq == seq
            {
                entry.dirty = false;
                self.enqueue_clean(offset, seq);
            }
        }
        Ok(())
    }

    /// Clear all entries from the cache and reset size tracking.
    pub async fn clear(&self) -> Result<()> {
        let _flush_guard = self.flush_gate.lock().await;
        let mut entries = self.entries.lock().await;

        let cleared_bytes: usize = entries.values().map(|e| e.size_bytes()).sum();
        entries.clear();
        self.clean_lru
            .lock()
            .expect("clean LRU lock is not poisoned")
            .clear();
        self.total_cached_bytes
            .fetch_sub(cleared_bytes, Ordering::Relaxed);

        debug!("Cleared cache ({} bytes)", cleared_bytes);
        Ok(())
    }

    pub(crate) fn enqueue_clean(&self, offset: u64, seq: u64) {
        self.clean_lru
            .lock()
            .expect("clean LRU lock is not poisoned")
            .push(Reverse((seq, offset)));
    }

    pub(crate) fn take_clean_lru_candidate(
        &self,
        entries: &BTreeMap<u64, CacheEntry>,
    ) -> Option<u64> {
        let mut lru = self
            .clean_lru
            .lock()
            .expect("clean LRU lock is not poisoned");
        while let Some(Reverse((seq, offset))) = lru.pop() {
            if entries
                .get(&offset)
                .is_some_and(|entry| !entry.dirty && entry.seq == seq)
            {
                return Some(offset);
            }
        }
        None
    }

    // The write-path and read-path methods are in their respective sub-modules.
    // Rust allows multiple impl blocks for the same type across modules
    // within the same crate, so write(), read(), flush() etc. are
    // still available on WrDiskCache instances as if they were defined here.
}
