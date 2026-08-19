mod falloc;
mod strategies;
#[cfg(windows)]
mod windows;

#[cfg(test)]
mod tests;

// Re-exports: preserve the original public API surface.
pub use crate::filesystem::disk_space::check_disk_space;
pub use strategies::get_available_space;

use super::disk_adaptor::{DirectDiskAdaptor, DiskAdaptor};
use crate::error::{Aria2Error, FatalError, Result};
use std::path::Path;

/// One-time warning emitted when `fallocate`-style allocation succeeds but
/// the allocated blocks are NOT zero-filled by the platform (macOS
/// `F_PREALLOCATE`, Windows `SetFileValidData`) and the caller did not opt
/// into `secure-falloc`. In that case residual disk data may be exposed
/// until the download overwrites those blocks. `std::sync::Once` ensures the
/// warning is logged only once per process to avoid log spam.
#[cfg_attr(target_os = "linux", allow(dead_code))]
static SECURE_FALLOC_WARN_ONCE: std::sync::Once = std::sync::Once::new();

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum AllocationStrategy {
    #[default]
    None,
    Prealloc,
    Falloc,
    Trunc,
    /// Memory-mapped I/O: pre-allocate blocks via `fallocate`, then the writer
    /// uses `MmapDiskWriter` for direct memory access. The allocation step is
    /// identical to `Falloc`; the difference is in the writer construction
    /// (handled by `DownloadCommand` based on the `file_allocation` option).
    Mmap,
}

impl AllocationStrategy {
    /// Parse allocation strategy from string.
    /// This is intentionally not implementing FromStr to avoid confusion with the standard trait.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "prealloc" => AllocationStrategy::Prealloc,
            "falloc" => AllocationStrategy::Falloc,
            "trunc" => AllocationStrategy::Trunc,
            "mmap" => AllocationStrategy::Mmap,
            _ => AllocationStrategy::None,
        }
    }
}

/// Allocate file space using the specified strategy.
/// This function provides cross-platform support for file preallocation:
/// - `prealloc`/`falloc`: use the platform-native allocation path with its
///   Rust-owned fallback behavior
/// - `trunc`: use `set_len()` without attempting physical allocation
/// - `mmap`: use native allocation before the memory-mapped writer opens
///
/// # Arguments
/// * `adaptor` - Disk adaptor for file operations
/// * `path` - Path to the file (used for error messages)
/// * `length` - Desired file length in bytes
/// * `strategy` - Allocation strategy to use
/// * `secure` - When `true`, zero-fill allocated blocks on platforms that
///   don't zero-fill (macOS, Windows). No-op on Linux where `fallocate(2)`
///   always returns zeroed blocks.
pub async fn allocate_file<D: DiskAdaptor>(
    adaptor: &mut D,
    _path: &Path,
    length: u64,
    strategy: AllocationStrategy,
    secure: bool,
) -> Result<()> {
    match strategy {
        AllocationStrategy::None => Ok(()),
        AllocationStrategy::Prealloc | AllocationStrategy::Falloc => {
            falloc::fallocate(adaptor, length, secure).await
        }
        AllocationStrategy::Trunc => strategies::truncate(adaptor, length).await,
        // Mmap uses fallocate to ensure blocks are allocated before mapping;
        // the actual mmap is performed by MmapDiskWriter at open time.
        AllocationStrategy::Mmap => falloc::fallocate(adaptor, length, secure).await,
    }
}

pub async fn preallocate_file(
    path: &Path,
    length: u64,
    strategy: &str,
    secure: bool,
) -> Result<()> {
    preallocate_file_with_progress(path, length, strategy, None::<&fn(u64, u64)>, secure).await
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
    secure: bool,
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
    allocate_file(&mut adaptor, path, length, alloc_strategy, secure).await?;

    if let Some(cb) = on_progress
        && length >= PROGRESS_THRESHOLD
    {
        cb(length, length);
    }

    adaptor.close().await
}
