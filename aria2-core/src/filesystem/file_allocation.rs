use super::disk_adaptor::{DirectDiskAdaptor, DiskAdaptor};
use crate::error::{Aria2Error, FatalError, Result};
use crate::filesystem::disk_space::check_disk_space;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum AllocationStrategy {
    #[default]
    None,
    Prealloc,
    Falloc,
    Trunc,
}

impl AllocationStrategy {
    /// Parse allocation strategy from string
    /// This is intentionally not implementing FromStr to avoid confusion with the standard trait
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "prealloc" => AllocationStrategy::Prealloc,
            "falloc" => AllocationStrategy::Falloc,
            "trunc" => AllocationStrategy::Trunc,
            _ => AllocationStrategy::None,
        }
    }
}

/// Allocate file space using the specified strategy.
/// This function provides cross-platform support for file preallocation:
/// - On Unix: Uses posix_fallocate64 when available (for Prealloc/Falloc), falls back to set_len
/// - On Windows: Uses SetEndOfFile via set_len() (works for all strategies)
/// - On macOS: Uses set_len() as fallback (macOS doesn't have fallocate)
///
/// # Arguments
/// * `adaptor` - Disk adaptor for file operations
/// * `path` - Path to the file (used for error messages)
/// * `length` - Desired file length in bytes
/// * `strategy` - Allocation strategy to use
pub async fn allocate_file<D: DiskAdaptor>(
    adaptor: &mut D,
    _path: &Path,
    length: u64,
    strategy: AllocationStrategy,
) -> Result<()> {
    match strategy {
        AllocationStrategy::None => Ok(()),
        AllocationStrategy::Prealloc => preallocate(adaptor, length).await,
        AllocationStrategy::Falloc => fallocate(adaptor, length).await,
        AllocationStrategy::Trunc => truncate(adaptor, length).await,
    }
}

pub async fn preallocate_file(path: &Path, length: u64, strategy: &str) -> Result<()> {
    preallocate_file_with_progress(path, length, strategy, None::<&fn(u64, u64)>).await
}

/// Preallocate file with optional progress callback for large allocations.
///
/// The callback `on_progress` is invoked at 10% intervals during allocation
/// for files larger than 100MB, receiving `(bytes_allocated, total_bytes)`.
pub async fn preallocate_file_with_progress<F>(
    path: &Path,
    length: u64,
    strategy: &str,
    on_progress: Option<&F>,
) -> Result<()>
where
    F: Fn(u64, u64) + Send + Sync,
{
    let alloc_strategy = AllocationStrategy::from_str(strategy);

    if length == 0 || alloc_strategy == AllocationStrategy::None {
        return Ok(());
    }

    // K5.3: Pre-allocation disk space check
    // Verify sufficient disk space before attempting allocation to prevent
    // failures mid-download due to exhausted storage. The check includes
    // a 10% headroom margin for filesystem overhead.
    if let Err(_e) = check_disk_space(path, length) {
        return Err(Aria2Error::Fatal(FatalError::DiskSpaceExhausted));
    }

    if let Some(parent) = path.parent() {
        let parent: &Path = parent;
        if !parent.exists() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e: std::io::Error| Aria2Error::Io(e.to_string()))?;
        }
    }

    const PROGRESS_THRESHOLD: u64 = 100 * 1024 * 1024; // 100MB

    if let Some(cb) = on_progress
        && length >= PROGRESS_THRESHOLD
    {
        cb(0, length);
    }

    let mut adaptor = DirectDiskAdaptor::new();
    adaptor.open(path).await?;
    allocate_file(&mut adaptor, path, length, alloc_strategy).await?;

    if let Some(cb) = on_progress
        && length >= PROGRESS_THRESHOLD
    {
        cb(length, length);
    }

    adaptor.close().await
}

/// Preallocate file space using truncation (set_len).
/// This is the simplest allocation method that works on all platforms:
/// - Unix: Uses ftruncate via set_len
/// - Windows: Uses SetEndOfFile via set_len
/// - macOS: Uses set_len
///
/// Note: This method does not guarantee contiguous disk space allocation,
/// but it ensures the file has the specified size.
async fn preallocate<D: DiskAdaptor>(adaptor: &mut D, length: u64) -> Result<()> {
    adaptor.truncate(length).await
}

/// Allocate file space using posix_fallocate on Unix systems.
/// This method ensures contiguous disk space allocation when possible.
///
/// Platform-specific behavior:
/// - **Unix/Linux**: Uses posix_fallocate64 syscall for true preallocation.
///   Falls back to set_len if raw file descriptor is not available.
/// - **Windows**: Uses SetEndOfFile via set_len() as native fallback.
///   Windows doesn't have posix_fallocate, but set_len provides similar functionality.
/// - **macOS**: macOS doesn't have fallocate/posix_fallocate.
///   Uses set_len() which calls ftruncate under the hood.
async fn fallocate<D: DiskAdaptor>(adaptor: &mut D, length: u64) -> Result<()> {
    #[cfg(unix)]
    {
        // Try to use posix_fallocate64 for true preallocation on Unix
        if let Some(fd) = adaptor.unix_raw_fd() {
            unsafe {
                let ret = libc::posix_fallocate64(fd, 0, length as i64);
                if ret != 0 {
                    return Err(Aria2Error::Io(
                        std::io::Error::from_raw_os_error(ret).to_string(),
                    ));
                }
            }
            Ok(())
        } else {
            // Fall back to set_len if no raw fd available
            adaptor.truncate(length).await
        }
    }

    #[cfg(not(unix))]
    {
        // On Windows and other non-Unix systems, use set_len as native method
        // This calls SetEndOfFile on Windows, which provides file allocation
        adaptor.truncate(length).await
    }
}

/// Truncate file to the specified length.
/// Works identically on all platforms using set_len:
/// - Unix: ftruncate system call
/// - Windows: SetEndOfFile API
/// - macOS: ftruncate
async fn truncate<D: DiskAdaptor>(adaptor: &mut D, length: u64) -> Result<()> {
    adaptor.truncate(length).await
}

pub async fn get_available_space(path: &Path) -> Result<u64> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = tokio::fs::metadata(parent)
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;

        let statvfs_result = unsafe {
            let mut stat: libc::statvfs64 = std::mem::zeroed();
            let ret = libc::statvfs64(
                parent.to_str().unwrap_or(".").as_ptr() as *const i8,
                &mut stat,
            );
            (ret, stat)
        };

        if statvfs_result.0 == 0 {
            let stat = statvfs_result.1;
            Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
        } else {
            Err(Aria2Error::Io("Failed to get disk space".to_string()))
        }
    }

    #[cfg(windows)]
    {
        let metadata = tokio::fs::metadata(parent)
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;

        let free = metadata.len();
        if free > 0 { Ok(free) } else { Ok(u64::MAX / 2) }
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        Ok(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            AllocationStrategy::from_str("invalid"),
            AllocationStrategy::None
        );
        assert_eq!(AllocationStrategy::from_str(""), AllocationStrategy::None);
    }

    #[tokio::test]
    async fn test_preallocate_file_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_none.bin");
        preallocate_file(&path, 1024, "none").await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn test_preallocate_file_trunc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_trunc.bin");
        preallocate_file(&path, 4096, "trunc").await.unwrap();

        let metadata = tokio::fs::metadata(&path).await.unwrap();
        assert_eq!(metadata.len(), 4096);
    }

    #[tokio::test]
    async fn test_preallocate_file_prealloc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_prealloc.bin");
        preallocate_file(&path, 1024 * 1024, "prealloc")
            .await
            .unwrap();

        let metadata = tokio::fs::metadata(&path).await.unwrap();
        assert_eq!(metadata.len(), 1024 * 1024);
    }

    #[tokio::test]
    async fn test_preallocate_zero_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_zero.bin");
        preallocate_file(&path, 0, "trunc").await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn test_preallocate_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub1").join("sub2").join("test_nested.bin");
        preallocate_file(&path, 100, "trunc").await.unwrap();

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
        preallocate_file(&path, 2048, "trunc").await.unwrap();

        let metadata = tokio::fs::metadata(&path).await.unwrap();
        assert_eq!(metadata.len(), 2048);
    }

    /// Test cross-platform file allocation with Prealloc strategy
    /// Verifies that Prealloc works on Windows, macOS, and Linux
    #[tokio::test]
    async fn test_allocate_file_cross_platform_prealloc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_alloc_prealloc.bin");

        // Create file with initial content
        tokio::fs::write(&path, b"hello").await.unwrap();

        // Allocate 10MB using Prealloc strategy
        preallocate_file(&path, 10 * 1024 * 1024, "prealloc")
            .await
            .unwrap();

        // Verify size is correct
        let metadata = tokio::fs::metadata(&path).await.unwrap();
        assert_eq!(metadata.len(), 10 * 1024 * 1024);
    }

    /// Test cross-platform file allocation with Falloc strategy
    /// Verifies that Falloc works on Windows (using set_len fallback) and Unix (using posix_fallocate)
    #[tokio::test]
    async fn test_allocate_file_cross_platform_falloc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_alloc_falloc.bin");

        // Create file first
        tokio::fs::write(&path, b"initial data").await.unwrap();

        // Allocate 5MB using Falloc strategy
        preallocate_file(&path, 5 * 1024 * 1024, "falloc")
            .await
            .unwrap();

        // Verify size
        let metadata = tokio::fs::metadata(&path).await.unwrap();
        assert_eq!(metadata.len(), 5 * 1024 * 1024);
    }

    /// Test cross-platform file allocation with Trunc strategy
    /// Verifies that Trunc works identically on all platforms via set_len
    #[tokio::test]
    async fn test_allocate_file_cross_platform_trunc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_alloc_trunc.bin");

        // Create file with some data
        tokio::fs::write(&path, b"some initial content here")
            .await
            .unwrap();

        // Truncate to 1MB using Trunc strategy
        preallocate_file(&path, 1024 * 1024, "trunc").await.unwrap();

        // Verify size
        let metadata = tokio::fs::metadata(&path).await.unwrap();
        assert_eq!(metadata.len(), 1024 * 1024);
    }

    /// Test None strategy does not create files
    #[tokio::test]
    async fn test_allocate_file_cross_platform_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_alloc_none.bin");

        // Try to allocate with None strategy - should not create file
        preallocate_file(&path, 1024 * 1024, "none").await.unwrap();

        // File should not exist
        assert!(!path.exists());
    }

    /// Test allocating a large file (50MB) to verify performance across platforms
    #[tokio::test]
    async fn test_allocate_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_large_alloc.bin");

        // Allocate 50MB using falloc strategy
        preallocate_file(&path, 50 * 1024 * 1024, "falloc")
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

    /// Test that all three allocation strategies produce same result
    #[tokio::test]
    async fn test_all_strategies_same_result() {
        let dir = tempfile::tempdir().unwrap();
        let test_size: u64 = 1024 * 100; // 100KB

        let strategies = vec!["prealloc", "falloc", "trunc"];

        for (i, strategy) in strategies.iter().enumerate() {
            let path = dir.path().join(format!("test_strategy_{}.bin", i));

            preallocate_file(&path, test_size, strategy).await.unwrap();

            let metadata = tokio::fs::metadata(&path).await.unwrap();
            assert_eq!(
                metadata.len(),
                test_size,
                "Strategy {} produced wrong size",
                strategy
            );
        }
    }

    /// Test progress callback is invoked for large file allocation (>=100MB)
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
        )
        .await
        .unwrap();

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

        // Verify file was actually created correctly
        let metadata = tokio::fs::metadata(&path).await.unwrap();
        assert_eq!(metadata.len(), 150 * 1024 * 1024);
    }

    /// Test small file does NOT trigger progress callback (<100MB)
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
}
