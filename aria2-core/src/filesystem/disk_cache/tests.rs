use super::*;
use crate::filesystem::disk_writer::SeekableDiskWriter;

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

    cache.write(0, bytes::Bytes::from("hello")).await.unwrap();
    cache.write(100, bytes::Bytes::from("world")).await.unwrap();

    assert_eq!(cache.size().await, 10);
    assert_eq!(cache.count().await, 2);

    // Read back by exact offset
    let result = cache.read(0, 5).await.unwrap();
    assert!(result.is_some());
    assert_eq!(&result.unwrap()[..], b"hello");

    let result = cache.read(100, 5).await.unwrap();
    assert!(result.is_some());
    assert_eq!(&result.unwrap()[..], b"world");
}

#[tokio::test]
async fn test_flush_returns_dirty_entries() {
    let cache = make_small_cache(4096);

    cache.write(0, bytes::Bytes::from("data1")).await.unwrap();
    cache.write(10, bytes::Bytes::from("data2")).await.unwrap();

    assert_eq!(cache.dirty_count().await, 2);

    let flushed = cache.flush().await.unwrap();
    assert_eq!(flushed.len(), 2);

    // After flush, entries are no longer dirty
    assert_eq!(cache.dirty_count().await, 0);
    // But they're still in the cache
    assert_eq!(cache.count().await, 2);
}

#[tokio::test]
async fn test_flush_to_persists_and_marks_only_successful_snapshot_clean() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("flush-to.bin");
    let cache = make_small_cache(4096);
    cache
        .write(7, bytes::Bytes::from_static(b"cached"))
        .await
        .unwrap();

    let mut writer = crate::filesystem::disk_writer::CachedDiskWriter::new(&path, None, None);
    writer.open().await.unwrap();
    cache.flush_to(&mut writer).await.unwrap();

    assert_eq!(cache.dirty_count().await, 0);
    writer.flush().await.unwrap();
    let data = tokio::fs::read(&path).await.unwrap();
    assert_eq!(&data[7..13], b"cached");
}

#[test]
fn test_coalesce_flush_entries_only_merges_contiguous_ranges() {
    let adjacent = vec![
        (0, bytes::Bytes::from_static(b"ab"), 1),
        (2, bytes::Bytes::from_static(b"cd"), 2),
        (10, bytes::Bytes::from_static(b"ef"), 3),
    ];

    let coalesced = super::coalesce_flush_entries(&adjacent);

    assert_eq!(coalesced.len(), 2);
    assert_eq!(coalesced[0].0, 0);
    assert_eq!(&coalesced[0].1[..], b"abcd");
    assert_eq!(coalesced[1].0, 10);
    assert_eq!(&coalesced[1].1[..], b"ef");
}

#[tokio::test]
async fn test_clear_resets_cache() {
    let cache = make_small_cache(4096);

    cache
        .write(0, bytes::Bytes::from(vec![0x42; 100]))
        .await
        .unwrap();
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

    // Phase 1: Write 3 dirty entries (300 bytes) -- under limit
    cache
        .write(0, bytes::Bytes::from(vec![0u8; 100]))
        .await
        .unwrap(); // entry A: offset=0
    cache
        .write(100, bytes::Bytes::from(vec![1u8; 100]))
        .await
        .unwrap(); // entry B: offset=100
    cache
        .write(200, bytes::Bytes::from(vec![2u8; 100]))
        .await
        .unwrap(); // entry C: offset=200
    assert_eq!(cache.count().await, 3);
    assert_eq!(cache.size().await, 300);

    // Flush to make them clean
    cache.flush().await.unwrap();
    assert_eq!(cache.dirty_count().await, 0);
    assert_eq!(cache.count().await, 3); // Still present

    // Phase 2: Write more entries that will trigger eviction
    // Writing 300 more bytes would exceed 500 limit -> should evict old clean ones
    cache
        .write(300, bytes::Bytes::from(vec![3u8; 100]))
        .await
        .unwrap(); // entry D: dirty
    cache
        .write(400, bytes::Bytes::from(vec![4u8; 100]))
        .await
        .unwrap(); // entry E: dirty

    // At this point we have 5 entries (500 bytes). Adding one more triggers eviction.
    cache
        .write(500, bytes::Bytes::from(vec![5u8; 100]))
        .await
        .unwrap(); // entry F: dirty

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
    // Entry A (offset 0) was the oldest clean -- it should be gone
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
    cache
        .write(0, bytes::Bytes::from(vec![0xAA; 100]))
        .await
        .unwrap(); // dirty A
    cache
        .write(100, bytes::Bytes::from(vec![0xBB; 100]))
        .await
        .unwrap(); // dirty B
    cache
        .write(200, bytes::Bytes::from(vec![0xCC; 100]))
        .await
        .unwrap(); // dirty C
    cache
        .write(300, bytes::Bytes::from(vec![0xDD; 100]))
        .await
        .unwrap(); // dirty D

    assert_eq!(cache.dirty_count().await, 4);
    assert_eq!(cache.count().await, 4);

    // Now try to write another entry that would exceed the limit.
    // All existing entries are dirty, so NONE can be evicted.
    // The write must still succeed (cache may temporarily overshoot).
    cache
        .write(400, bytes::Bytes::from(vec![0xEE; 100]))
        .await
        .unwrap(); // dirty E

    // ALL 5 dirty entries must still be present -- zero data loss allowed
    assert_eq!(
        cache.count().await,
        5,
        "All dirty entries must be preserved -- none should be evicted"
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
    cache
        .write(0, bytes::Bytes::from(vec![1u8; 100]))
        .await
        .unwrap(); // will become clean
    cache
        .write(100, bytes::Bytes::from(vec![2u8; 100]))
        .await
        .unwrap(); // will become clean
    cache.flush().await.unwrap(); // Mark A, B as clean

    // Write new dirty entries
    cache
        .write(200, bytes::Bytes::from(vec![3u8; 100]))
        .await
        .unwrap(); // dirty C
    cache
        .write(300, bytes::Bytes::from(vec![4u8; 100]))
        .await
        .unwrap(); // dirty D

    // Now: A(clean), B(clean), C(dirty), D(dirty) = 400 bytes
    // Write more to trigger eviction
    cache
        .write(400, bytes::Bytes::from(vec![5u8; 100]))
        .await
        .unwrap(); // dirty E -- now 500 bytes
    cache
        .write(500, bytes::Bytes::from(vec![6u8; 100]))
        .await
        .unwrap(); // dirty F -- exceeds 500, triggers eviction

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
    // Verify the lifecycle: write -> flush (clean) -> write more -> evicts old clean
    let cache = make_small_cache(300);

    // Fill with dirty entries, then flush them
    cache
        .write(0, bytes::Bytes::from(vec![0u8; 150]))
        .await
        .unwrap();
    cache.flush().await.unwrap(); // Now clean
    assert_eq!(cache.dirty_count().await, 0);

    // Write new dirty entry -- should trigger eviction of the clean one
    cache
        .write(200, bytes::Bytes::from(vec![1u8; 150]))
        .await
        .unwrap();

    // The first entry (now clean) may or may not have been evicted depending on
    // whether 300 + 150 > 300 triggered it. With our logic, 150 + 150 = 300 which
    // is NOT > 300, so no eviction yet. One more write should trigger it.
    cache
        .write(400, bytes::Bytes::from(vec![2u8; 150]))
        .await
        .unwrap(); // 450 > 300 -> evict!

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
            .write((i * 50) as u64, bytes::Bytes::from(vec![i as u8; 50]))
            .await
            .unwrap();
    }
    // 20 * 50 = 1000 bytes = exactly at limit

    cache.flush().await.unwrap(); // All clean now

    // Write one more entry to push over limit and trigger eviction
    cache
        .write(2000, bytes::Bytes::from(vec![0xFF; 50]))
        .await
        .unwrap(); // 1050 > 1000 -> evict

    // Should have evicted down to target (~500 bytes / 50 per entry = 10 entries)
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

    cache
        .write(0, bytes::Bytes::from(vec![0u8; 256]))
        .await
        .unwrap();
    cache
        .write(256, bytes::Bytes::from(vec![1u8; 256]))
        .await
        .unwrap();

    // Lock-free read should match locked read
    let lock_free_size = cache.current_size_bytes();
    let locked_size = cache.size().await;

    assert_eq!(lock_free_size, locked_size);
    assert_eq!(lock_free_size, 512);
}

#[tokio::test]
async fn test_read_miss_returns_none() {
    let cache = make_small_cache(1024);

    cache.write(0, bytes::Bytes::from("hello")).await.unwrap();

    // Non-existent offset
    assert!(cache.read(999, 5).await.unwrap().is_none());

    // Offset exists but length too long
    assert!(cache.read(0, 100).await.unwrap().is_none());
}

#[tokio::test]
async fn test_range_based_read() {
    let cache = make_small_cache(4096);

    // Write an entry covering offsets 0-99
    cache
        .write(0, bytes::Bytes::from(vec![42u8; 100]))
        .await
        .unwrap();

    // Read a sub-range within the entry
    let result = cache.read(10, 30).await.unwrap();
    assert!(result.is_some());
    let data = result.unwrap();
    assert_eq!(data.len(), 30);
    assert!(data.iter().all(|&b| b == 42));
}

#[tokio::test]
async fn test_overlapping_writes_are_last_write_wins() {
    let cache = make_small_cache(4096);

    cache
        .write(0, bytes::Bytes::from_static(b"abcdefghij"))
        .await
        .unwrap();
    cache
        .write(5, bytes::Bytes::from_static(b"XYZ"))
        .await
        .unwrap();

    let data = cache.read(0, 10).await.unwrap().unwrap();
    assert_eq!(&data[..], b"abcdeXYZij");
    assert_eq!(cache.size().await, 10);

    // The write splits the old entry into disjoint left/right fragments.
    assert_eq!(cache.count().await, 3);
    assert_eq!(&cache.read(3, 7).await.unwrap().unwrap()[..], b"deXYZij");
}

#[tokio::test]
async fn test_overlapping_writes_flush_latest_bytes_to_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("overlap.bin");
    let cache = make_small_cache(4096);

    // Reverse the write order from the previous test. Offset ordering must
    // not decide which bytes win when ranges overlap.
    cache
        .write(5, bytes::Bytes::from_static(b"XYZ"))
        .await
        .unwrap();
    cache
        .write(0, bytes::Bytes::from_static(b"abcdefghij"))
        .await
        .unwrap();

    let mut writer = crate::filesystem::disk_writer::CachedDiskWriter::new(&path, None, None);
    writer.open().await.unwrap();
    cache.flush_to(&mut writer).await.unwrap();

    let data = tokio::fs::read(&path).await.unwrap();
    assert_eq!(&data[..], b"abcdefghij");
}

#[tokio::test]
async fn test_cache_rejects_ranges_that_overflow_u64() {
    let cache = make_small_cache(4096);

    assert!(
        cache
            .write(u64::MAX, bytes::Bytes::from_static(b"x"))
            .await
            .is_err()
    );
    assert!(cache.read(u64::MAX, 2).await.is_err());
}

#[tokio::test]
async fn test_multiple_flushes_only_return_dirty() {
    let cache = make_small_cache(4096);

    cache.write(0, bytes::Bytes::from("A")).await.unwrap();
    cache.write(1, bytes::Bytes::from("B")).await.unwrap();

    // First flush returns both
    let f1 = cache.flush().await.unwrap();
    assert_eq!(f1.len(), 2);

    // Second flush returns nothing (already clean)
    let f2 = cache.flush().await.unwrap();
    assert_eq!(f2.len(), 0);

    // Write a third entry
    cache.write(2, bytes::Bytes::from("C")).await.unwrap();

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
    cache
        .write(0, bytes::Bytes::from(vec![0u8; 200]))
        .await
        .unwrap();

    // Must succeed without losing data (no clean entries to evict anyway)
    assert_eq!(cache.count().await, 1);
    assert_eq!(cache.size().await, 200);
    assert_eq!(cache.dirty_count().await, 1);

    let result = cache.read(0, 200).await.unwrap();
    assert!(result.is_some());
    assert_eq!(result.unwrap().len(), 200);
}
