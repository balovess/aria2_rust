use super::*;
use crate::filesystem::disk_adaptor::DirectDiskAdaptor;

#[test]
fn test_allocation_strategy_from_str() {
    assert_eq!(
        AllocationStrategy::from_str("none"),
        AllocationStrategy::None
    );
    assert_eq!(
        AllocationStrategy::from_str("prealloc"),
        AllocationStrategy::Prealloc
    );
    assert_eq!(
        AllocationStrategy::from_str("falloc"),
        AllocationStrategy::Falloc
    );
    assert_eq!(
        AllocationStrategy::from_str("trunc"),
        AllocationStrategy::Trunc
    );
    assert_eq!(
        AllocationStrategy::from_str("mmap"),
        AllocationStrategy::Mmap
    );
    assert_eq!(
        AllocationStrategy::from_str("invalid"),
        AllocationStrategy::None
    );
    assert_eq!(AllocationStrategy::from_str(""), AllocationStrategy::None);
}

#[tokio::test]
async fn test_preallocate_file_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_none.bin");
    preallocate_file(&path, 1024, "none", false).await.unwrap();
    assert!(!path.exists());
}

#[tokio::test]
async fn test_preallocate_file_trunc() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_trunc.bin");
    preallocate_file(&path, 4096, "trunc", false).await.unwrap();

    let metadata = tokio::fs::metadata(&path).await.unwrap();
    assert_eq!(metadata.len(), 4096);
}

#[tokio::test]
async fn test_preallocate_file_prealloc() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_prealloc.bin");
    preallocate_file(&path, 1024 * 1024, "prealloc", false)
        .await
        .unwrap();

    let metadata = tokio::fs::metadata(&path).await.unwrap();
    assert_eq!(metadata.len(), 1024 * 1024);
}

#[tokio::test]
async fn test_preallocate_file_prealloc_preserves_existing_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_prealloc_resume.bin");
    let prefix = b"resume data must survive native allocation";
    tokio::fs::write(&path, prefix).await.unwrap();

    preallocate_file(&path, 1024 * 1024, "prealloc", false)
        .await
        .unwrap();

    let content = tokio::fs::read(&path).await.unwrap();
    assert_eq!(&content[..prefix.len()], prefix);
    assert_eq!(content.len(), 1024 * 1024);
}

#[tokio::test]
async fn test_preallocate_zero_length() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_zero.bin");
    preallocate_file(&path, 0, "trunc", false).await.unwrap();
    assert!(!path.exists());
}

#[tokio::test]
async fn test_preallocate_creates_parent_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub1").join("sub2").join("test_nested.bin");
    preallocate_file(&path, 100, "trunc", false).await.unwrap();

    assert!(path.exists());
    let metadata = tokio::fs::metadata(&path).await.unwrap();
    assert_eq!(metadata.len(), 100);
}

#[tokio::test]
async fn test_get_available_space_returns_value() {
    let dir = tempfile::tempdir().unwrap();
    let space = get_available_space(dir.path()).await;
    assert!(space.is_ok());
    let val = space.unwrap();
    assert!(val > 0);
}

#[tokio::test]
async fn test_preallocate_overwrite_existing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_overwrite.bin");

    tokio::fs::write(&path, b"original data").await.unwrap();
    preallocate_file(&path, 2048, "trunc", false).await.unwrap();

    let metadata = tokio::fs::metadata(&path).await.unwrap();
    assert_eq!(metadata.len(), 2048);
}

/// Test cross-platform file allocation with Prealloc strategy.
/// Verifies that Prealloc works on Windows, macOS, and Linux.
#[tokio::test]
async fn test_allocate_file_cross_platform_prealloc() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_alloc_prealloc.bin");

    // Create file with initial content
    tokio::fs::write(&path, b"hello").await.unwrap();

    // Allocate 10MB using Prealloc strategy
    preallocate_file(&path, 10 * 1024 * 1024, "prealloc", false)
        .await
        .unwrap();

    // Verify size is correct
    let metadata = tokio::fs::metadata(&path).await.unwrap();
    assert_eq!(metadata.len(), 10 * 1024 * 1024);
}

/// Test cross-platform file allocation with Falloc strategy.
/// Verifies that Falloc works on Windows (using set_len fallback) and Unix (using posix_fallocate).
#[tokio::test]
async fn test_allocate_file_cross_platform_falloc() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_alloc_falloc.bin");

    // Create file first
    tokio::fs::write(&path, b"initial data").await.unwrap();

    // Allocate 5MB using Falloc strategy
    preallocate_file(&path, 5 * 1024 * 1024, "falloc", false)
        .await
        .unwrap();

    // Verify size
    let metadata = tokio::fs::metadata(&path).await.unwrap();
    assert_eq!(metadata.len(), 5 * 1024 * 1024);
}

/// Test that the Falloc strategy produces a file of the correct logical size.
///
/// On Windows the file is sparse unless `SetFileValidData` succeeds (which
/// requires `SE_MANAGE_VOLUME_PRIVILEGE`, typically absent in test runs).
/// On Unix, `posix_fallocate`/`F_PREALLOCATE` allocate real blocks when
/// supported. This test only asserts the logical size, keeping it
/// cross-platform and independent of privilege state.
#[tokio::test]
async fn test_fallocate_creates_sparse_file_of_correct_size() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_falloc_sparse.bin");

    preallocate_file(&path, 1024 * 1024, "falloc", false)
        .await
        .unwrap();

    let metadata = tokio::fs::metadata(&path).await.unwrap();
    assert_eq!(metadata.len(), 1024 * 1024);
}

/// Test cross-platform file allocation with Trunc strategy.
/// Verifies that Trunc works identically on all platforms via set_len.
#[tokio::test]
async fn test_allocate_file_cross_platform_trunc() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_alloc_trunc.bin");

    // Create file with some data
    tokio::fs::write(&path, b"some initial content here")
        .await
        .unwrap();

    // Truncate to 1MB using Trunc strategy
    preallocate_file(&path, 1024 * 1024, "trunc", false)
        .await
        .unwrap();

    // Verify size
    let metadata = tokio::fs::metadata(&path).await.unwrap();
    assert_eq!(metadata.len(), 1024 * 1024);
}

/// Test None strategy does not create files.
#[tokio::test]
async fn test_allocate_file_cross_platform_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_alloc_none.bin");

    // Try to allocate with None strategy - should not create file
    preallocate_file(&path, 1024 * 1024, "none", false)
        .await
        .unwrap();

    // File should not exist
    assert!(!path.exists());
}

/// Test allocating a large file (50MB) to verify performance across platforms.
#[tokio::test]
async fn test_allocate_large_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_large_alloc.bin");

    // Allocate 50MB using falloc strategy
    preallocate_file(&path, 50 * 1024 * 1024, "falloc", false)
        .await
        .unwrap();

    // Verify size
    let metadata = tokio::fs::metadata(&path).await.unwrap();
    assert_eq!(metadata.len(), 50 * 1024 * 1024);

    // Verify we can write to the allocated space
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .await
        .unwrap();

    // Write at an offset near the end of the file
    file.seek(std::io::SeekFrom::Start(49 * 1024 * 1024))
        .await
        .unwrap();
    file.write_all(b"end marker").await.unwrap();
    file.flush().await.unwrap();
    drop(file);

    // Verify final size unchanged
    let metadata = tokio::fs::metadata(&path).await.unwrap();
    assert_eq!(metadata.len(), 50 * 1024 * 1024);
}

/// Test that all three allocation strategies produce same result.
#[tokio::test]
async fn test_all_strategies_same_result() {
    let dir = tempfile::tempdir().unwrap();
    let test_size: u64 = 1024 * 100; // 100KB

    let strategies = ["prealloc", "falloc", "trunc"];

    for (i, strategy) in strategies.iter().enumerate() {
        let path = dir.path().join(format!("test_strategy_{}.bin", i));

        preallocate_file(&path, test_size, strategy, false)
            .await
            .unwrap();

        let metadata = tokio::fs::metadata(&path).await.unwrap();
        assert_eq!(
            metadata.len(),
            test_size,
            "Strategy {} produced wrong size",
            strategy
        );
    }
}

/// Test progress callback is invoked for large file allocation (>=100MB).
#[tokio::test]
async fn test_preallocate_with_progress_callback() {
    use std::sync::{Arc, Mutex};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_progress.bin");

    let progress_calls: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let pc = progress_calls.clone();

    preallocate_file_with_progress(
        &path,
        150 * 1024 * 1024, // 150MB — exceeds 100MB threshold
        "prealloc",
        Some(&|allocated, total| {
            pc.lock().unwrap().push((allocated, total));
        }),
        false,
    )
    .await
    .unwrap();

    {
        // Lock scope - must be dropped before await below
        let calls = progress_calls.lock().unwrap();
        // Should have at least start(0) and end(total) calls
        assert!(
            calls.len() >= 2,
            "expected at least 2 progress calls, got {}",
            calls.len()
        );
        assert_eq!(calls.first().unwrap().0, 0); // Start: 0 bytes
        assert_eq!(
            calls.last().unwrap().0,
            150 * 1024 * 1024 // End: full size
        );
        assert_eq!(calls.last().unwrap().1, 150 * 1024 * 1024);
    } // lock dropped here

    // Verify file was actually created correctly
    let metadata = tokio::fs::metadata(&path).await.unwrap();
    assert_eq!(metadata.len(), 150 * 1024 * 1024);
}

/// Test small file does NOT trigger progress callback (<100MB).
#[tokio::test]
async fn test_preallocate_small_file_no_progress_callback() {
    use std::sync::{Arc, Mutex};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_small_progress.bin");

    let progress_calls: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let pc = progress_calls.clone();

    preallocate_file_with_progress(
        &path,
        1024, // 1KB — well under 100MB threshold
        "trunc",
        Some(&|allocated, total| {
            pc.lock().unwrap().push((allocated, total));
        }),
        false,
    )
    .await
    .unwrap();

    let calls = progress_calls.lock().unwrap();
    // Small files should NOT trigger callback
    assert!(
        calls.is_empty(),
        "small file should not trigger progress, got {} calls",
        calls.len()
    );
}

/// Verify the workspace default file allocation strategy matches aria2.
#[test]
fn test_default_allocation_is_prealloc() {
    use crate::constants;
    assert_eq!(constants::DEFAULT_FILE_ALLOCATION, "prealloc");
    assert_eq!(
        AllocationStrategy::from_str(constants::DEFAULT_FILE_ALLOCATION),
        AllocationStrategy::Prealloc
    );
}

/// Test that async_zero_fill produces a file of the correct size filled
/// with zeros.
#[tokio::test]
async fn test_async_zero_fill() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_zero_fill.bin");

    let mut adaptor = DirectDiskAdaptor::new();
    adaptor.open(&path).await.unwrap();
    adaptor.truncate(5 * 1024 * 1024).await.unwrap(); // 5 MiB
    strategies::async_zero_fill(&mut adaptor, 5 * 1024 * 1024)
        .await
        .unwrap();
    adaptor.close().await.unwrap();

    // Verify size
    let metadata = tokio::fs::metadata(&path).await.unwrap();
    assert_eq!(metadata.len(), 5 * 1024 * 1024);

    // Verify content is all zeros
    let content = tokio::fs::read(&path).await.unwrap();
    assert!(content.iter().all(|&b| b == 0), "File should be all zeros");
}

/// A resumed allocation clears only the newly extended region.
#[tokio::test]
async fn test_async_zero_fill_from_preserves_existing_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_zero_fill_resume.bin");

    let prefix = vec![0x5Au8; 4096];
    tokio::fs::write(&path, &prefix).await.unwrap();

    let mut adaptor = DirectDiskAdaptor::new();
    adaptor.open(&path).await.unwrap();
    adaptor.truncate(8192).await.unwrap();
    strategies::async_zero_fill_from(&mut adaptor, 4096, 8192)
        .await
        .unwrap();
    adaptor.close().await.unwrap();

    let content = tokio::fs::read(&path).await.unwrap();
    assert_eq!(&content[..prefix.len()], prefix.as_slice());
    assert!(content[4096..].iter().all(|&b| b == 0));
}

/// Test that secure_falloc option defaults to false in DownloadOptions.
#[test]
fn test_secure_falloc_default() {
    use crate::request::request_group::DownloadOptions;
    let opts = DownloadOptions::default();
    assert!(!opts.secure_falloc, "secure_falloc should default to false");
}
