use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::sync::Mutex;
use tracing::debug;

use crate::error::Result;

/// Default maximum cache size: 16 MB
const DEFAULT_MAX_SIZE_BYTES: usize = 16 * 1024 * 1024;

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
    /// Maximum allowed cache size in bytes
    pub(crate) max_size_bytes: usize,
    /// Current total cached data size in bytes (atomic for lock-free reads)
    pub(crate) total_cached_bytes: AtomicUsize,
    /// Monotonic counter assigning insertion sequence numbers for LRU ordering.
    pub(crate) next_seq: AtomicU64,
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
        let max_size_bytes = max_size_mb * 1024 * 1024;

        debug!(
            "Initializing write-back disk cache, max capacity: {} MB ({} bytes)",
            max_size_mb, max_size_bytes
        );

        WrDiskCache {
            entries: Mutex::new(BTreeMap::new()),
            max_size_bytes,
            total_cached_bytes: AtomicUsize::new(0),
            next_seq: AtomicU64::new(0),
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
            max_size_bytes,
            total_cached_bytes: AtomicUsize::new(0),
            next_seq: AtomicU64::new(0),
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

    /// Clear all entries from the cache and reset size tracking.
    pub async fn clear(&self) -> Result<()> {
        let mut entries = self.entries.lock().await;

        let cleared_bytes: usize = entries.values().map(|e| e.size_bytes()).sum();
        entries.clear();
        self.total_cached_bytes
            .fetch_sub(cleared_bytes, Ordering::Relaxed);

        debug!("Cleared cache ({} bytes)", cleared_bytes);
        Ok(())
    }

    // The write-path and read-path methods are in their respective sub-modules.
    // Rust allows multiple impl blocks for the same type across modules
    // within the same crate, so write(), read(), flush() etc. are
    // still available on WrDiskCache instances as if they were defined here.
}
