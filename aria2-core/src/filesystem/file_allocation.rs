use super::disk_adaptor::{DirectDiskAdaptor, DiskAdaptor};
use crate::error::{Aria2Error, FatalError, Result};
use crate::filesystem::disk_space::check_disk_space;
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
    /// Parse allocation strategy from string
    /// This is intentionally not implementing FromStr to avoid confusion with the standard trait
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
/// - On Unix: Uses posix_fallocate64 when available (for Prealloc/Falloc), falls back to set_len
/// - On Windows: Uses SetEndOfFile via set_len() (works for all strategies)
/// - On macOS: Uses set_len() as fallback (macOS doesn't have fallocate)
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
        AllocationStrategy::Prealloc => preallocate(adaptor, length).await,
        AllocationStrategy::Falloc => fallocate(adaptor, length, secure).await,
        AllocationStrategy::Trunc => truncate(adaptor, length).await,
        // Mmap uses fallocate to ensure blocks are allocated before mapping;
        // the actual mmap is performed by MmapDiskWriter at open time.
        AllocationStrategy::Mmap => fallocate(adaptor, length, secure).await,
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

/// Zero-fill a file region in async 1 MiB chunks.
///
/// This is used as a fallback when `fallocate(2)` returns `EOPNOTSUPP`
/// (Linux) or as a security measure after `SetFileValidData`/`F_PREALLOCATE`
/// (Windows/macOS) which don't zero-fill the allocated blocks.
///
/// Uses `tokio::task::yield_now()` between chunks to avoid blocking the
/// reactor. The zero buffer is allocated once and reused.
async fn async_zero_fill<D: DiskAdaptor>(adaptor: &mut D, length: u64) -> Result<()> {
    const CHUNK_SIZE: usize = 1024 * 1024; // 1 MiB
    let zero_chunk = vec![0u8; CHUNK_SIZE];
    let mut remaining = length;
    let mut offset: u64 = 0;

    while remaining > 0 {
        let write_len = remaining.min(CHUNK_SIZE as u64) as usize;
        adaptor.write(offset, &zero_chunk[..write_len]).await?;
        offset += write_len as u64;
        remaining -= write_len as u64;

        // Cooperative yield to avoid starving other tasks
        tokio::task::yield_now().await;
    }

    Ok(())
}

/// Try to enable `SE_MANAGE_VOLUME_PRIVILEGE` in the process token.
///
/// `SetFileValidData` requires this privilege to allocate real disk blocks
/// (rather than sparse holes). For processes running with administrator
/// rights, the privilege exists in the token but is disabled by default —
/// this function enables it. For non-admin processes, the privilege is not
/// present, and the function returns `false` (the caller falls back to sparse
/// files, which is the correct behavior for that case).
///
/// Returns `true` if the privilege was successfully enabled, `false` otherwise.
#[cfg(windows)]
fn try_enable_volume_privilege() -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW,
        SE_MANAGE_VOLUME_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
        TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token_handle = std::ptr::null_mut();
    let result = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token_handle,
        )
    };
    if result == 0 {
        return false;
    }

    let mut luid: windows_sys::Win32::Foundation::LUID = unsafe { std::mem::zeroed() };
    if unsafe { LookupPrivilegeValueW(std::ptr::null(), SE_MANAGE_VOLUME_NAME, &mut luid) } == 0 {
        unsafe {
            CloseHandle(token_handle);
        }
        return false;
    }

    let mut tp: TOKEN_PRIVILEGES = unsafe { std::mem::zeroed() };
    tp.PrivilegeCount = 1;
    tp.Privileges[0].Luid = luid;
    tp.Privileges[0].Attributes = 2; // SE_PRIVILEGE_ENABLED

    let ret = unsafe {
        AdjustTokenPrivileges(
            token_handle,
            0,
            &tp,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    unsafe {
        CloseHandle(token_handle);
    }
    ret != 0
}

/// Allocate file space using platform-native preallocation syscalls.
/// This method attempts true disk-space allocation (avoiding sparse files)
/// when the platform and filesystem support it, with graceful fallbacks.
///
/// Platform-specific behavior:
/// - **Linux**: Uses raw `fallocate(2)` (NOT `posix_fallocate64`) so that
///   `EOPNOTSUPP` from the filesystem can be detected explicitly. When the
///   filesystem doesn't support `fallocate(2)`, we fall back to
///   `async_zero_fill` (cooperative zero-fill) instead of letting
///   `posix_fallocate64` block the async runtime with its internal
///   zero-fill loop. `fallocate(2)` always returns zeroed blocks on success,
///   so `secure` has no effect on Linux. Falls back to `set_len` if no raw
///   file descriptor is available.
/// - **macOS**: Uses `fcntl(F_PREALLOCATE)` with `F_ALLOCATEALL` for true space
///   allocation. `F_PREALLOCATE` does not zero-fill the allocated blocks, so
///   when `secure == true` we additionally run `async_zero_fill`. When
///   `secure == false`, a one-time warning is emitted (residual disk data
///   may be exposed). `F_PREALLOCATE` does not extend the file size, so the
///   file is sized first via `ftruncate` (set_len). Falls back to the sparse
///   `set_len` result if the raw fd is unavailable.
/// - **Windows**: Attempts `SetFileValidData` to extend the valid data length
///   and force allocation. `SetFileValidData` does NOT zero-fill, so when
///   `secure == true` we additionally run `async_zero_fill`. When
///   `secure == false`, a one-time warning is emitted. Requires
///   `SE_MANAGE_VOLUME_PRIVILEGE`; if the privilege is not held (or the call
///   fails for any other reason), it falls back to the sparse file produced
///   by `set_len` (SetEndOfFile).
/// - **Other Unix (BSD, etc.)**: No portable preallocate syscall; uses `set_len`.
#[cfg_attr(target_os = "linux", allow(unused_variables))]
async fn fallocate<D: DiskAdaptor>(adaptor: &mut D, length: u64, secure: bool) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if let Some(fd) = adaptor.unix_raw_fd() {
            // Use raw fallocate(2) to detect EOPNOTSUPP explicitly.
            // posix_fallocate64 would silently fall back to a blocking
            // zero-fill loop inside libc when the kernel returns EOPNOTSUPP,
            // which would stall the async runtime. By calling fallocate(2)
            // directly we can fall back to our own cooperative async_zero_fill.
            //
            // FALLOC_FL_NONE (0) requests default behavior: allocate space
            // and zero-fill it at the filesystem block level.
            let ret = unsafe {
                libc::fallocate(
                    fd,
                    // FALLOC_FL_NONE is 0 in the Linux kernel headers but is
                    // not exported by the libc crate. Literal 0 == default
                    // allocate-and-zero-fill behavior.
                    0 as libc::c_int,
                    0,
                    length as libc::off_t,
                )
            };
            if ret == 0 {
                // Success: kernel allocates zeroed blocks; secure is a no-op.
                return Ok(());
            }
            let errno = unsafe { *libc::__errno_location() };
            if errno == libc::EOPNOTSUPP {
                tracing::warn!(
                    length,
                    "fallocate(2) not supported by filesystem; \
                     falling back to async zero-fill"
                );
                // Size the file first so writes land at correct offsets.
                adaptor.truncate(length).await?;
                return async_zero_fill(adaptor, length).await;
            }
            // Other errors: return as I/O error
            Err(Aria2Error::Io(
                std::io::Error::from_raw_os_error(errno).to_string(),
            ))
        } else {
            // Fall back to set_len if no raw fd available
            adaptor.truncate(length).await
        }
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // macOS-only: untested on this Windows dev machine, validated via
        // type-checking only. `libc::fstore_t` and the F_* constants are
        // exposed by the libc crate on macOS targets.
        #[cfg(target_os = "macos")]
        {
            match adaptor.unix_raw_fd() {
                Some(fd) => {
                    // F_PREALLOCATE does not change the file size; size it first
                    // via ftruncate so the file is correct even if preallocation
                    // is rejected by the filesystem.
                    adaptor.truncate(length).await?;
                    // F_ALLOCATEALL allocates all requested space;
                    // F_PEOFPOSMODE measures offset from physical end of file.
                    let fstore = libc::fstore_t {
                        fst_flags: libc::F_ALLOCATEALL as libc::c_uint,
                        fst_posmode: libc::F_PEOFPOSMODE,
                        fst_offset: 0,
                        fst_length: length as libc::off_t,
                        fst_bytesalloc: 0,
                    };
                    let ret = unsafe { libc::fcntl(fd, libc::F_PREALLOCATE, &fstore) };
                    if ret != 0 {
                        // Filesystem may not support F_PREALLOCATE; the file is
                        // already sized above, so it remains sparse but correct.
                        tracing::warn!(
                            length,
                            "F_PREALLOCATE failed on macOS, file remains sparse"
                        );
                        return Ok(());
                    }
                    // F_PREALLOCATE succeeded but does NOT zero-fill the
                    // allocated blocks. Zero-fill when secure is requested,
                    // otherwise emit a one-time warning about the trade-off.
                    if secure {
                        async_zero_fill(adaptor, length).await
                    } else {
                        SECURE_FALLOC_WARN_ONCE.call_once(|| {
                            tracing::warn!(
                                "F_PREALLOCATE succeeded but does not zero-fill; \
                                 residual disk data may be exposed. \
                                 Enable --secure-falloc=true to zero-fill (at a \
                                 performance cost). This warning is logged once."
                            );
                        });
                        Ok(())
                    }
                }
                None => adaptor.truncate(length).await,
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            // Other Unix (BSD, etc.): no portable preallocate syscall; use
            // ftruncate via set_len which is the standard approach.
            adaptor.truncate(length).await
        }
    }

    #[cfg(not(unix))]
    {
        // Windows: attempt SetFileValidData for true space allocation.
        // Requires SE_MANAGE_VOLUME_PRIVILEGE; fall back to sparse set_len
        // (SetEndOfFile) when the privilege is not held or the call fails.
        //
        // NOTE: The raw HANDLE (*mut c_void) is not `Send`, so it must NOT be
        // held across an await point (the future is required to be `Send` by
        // callers). We therefore size the file first via `truncate` (which does
        // not need the handle), then fetch the handle and invoke
        // SetFileValidData synchronously with no await while the handle is live.
        // The zero-fill (which may await) happens AFTER the handle goes out of
        // scope, using a boolean flag to carry the result across the scope
        // boundary.
        adaptor.truncate(length).await?;
        // Attempt to enable SE_MANAGE_VOLUME_PRIVILEGE — this exists in the
        // token for admin processes but is disabled by default. Non-admin
        // processes will fail here and correctly fall back to sparse files.
        let _ = try_enable_volume_privilege();
        let valid_data_succeeded: bool = if let Some(handle) = adaptor.windows_raw_handle() {
            // Extend the valid data length up to `length`, forcing the
            // filesystem to allocate real blocks rather than a sparse hole.
            let ok = unsafe {
                windows_sys::Win32::Storage::FileSystem::SetFileValidData(handle, length as i64)
            };
            if ok == 0 {
                let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
                if err == windows_sys::Win32::Foundation::ERROR_PRIVILEGE_NOT_HELD {
                    // Promote to warn: SetFileValidData failure causes a 2x I/O
                    // penalty because the writer must zero-fill every block on
                    // first write (sparse file allocation on Windows).
                    tracing::warn!(
                        length,
                        "SetFileValidData requires SE_MANAGE_VOLUME_PRIVILEGE; \
                         falling back to sparse file. This causes ~2x I/O on \
                         first write because each block must be zeroed by the \
                         writer instead of being pre-allocated."
                    );
                } else {
                    tracing::warn!(length, err, "SetFileValidData failed; file remains sparse");
                }
                false
            } else {
                true
            }
        } else {
            false
        };

        // SetFileValidData succeeded but does NOT zero-fill the allocated
        // blocks (it exposes whatever was previously on disk). Zero-fill when
        // secure is requested, otherwise emit a one-time warning about the
        // trade-off. This block is outside the handle scope so the await is
        // safe (no non-Send raw handle is live across the suspension point).
        if valid_data_succeeded {
            if secure {
                async_zero_fill(adaptor, length).await
            } else {
                SECURE_FALLOC_WARN_ONCE.call_once(|| {
                    tracing::warn!(
                        "SetFileValidData succeeded but does not zero-fill; \
                         residual disk data may be exposed. \
                         Enable --secure-falloc=true to zero-fill (at a \
                         performance cost). This warning is logged once."
                    );
                });
                Ok(())
            }
        } else {
            Ok(())
        }
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

    #[cfg(target_os = "linux")]
    {
        let _metadata = tokio::fs::metadata(parent)
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

    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // On macOS and other Unix systems, use statvfs (not statvfs64)
        // macOS statvfs already handles large files
        let _metadata = tokio::fs::metadata(parent)
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;

        let statvfs_result = unsafe {
            let mut stat: libc::statvfs = std::mem::zeroed();
            let ret = libc::statvfs(
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
        use std::os::windows::ffi::OsStrExt;

        // Get absolute path for GetDiskFreeSpaceEx
        let abs_path = match std::fs::canonicalize(parent) {
            Ok(p) => p,
            Err(_) => {
                std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf())
            }
        };

        // Convert path to wide string with null terminator
        let mut wide_path: Vec<u16> = abs_path.as_os_str().encode_wide().collect();
        wide_path.push(0);

        let mut free_bytes_available: u64 = 0;
        let result = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide_path.as_ptr(),
                &mut free_bytes_available as *mut u64 as *mut _,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };

        if result == 0 {
            Err(Aria2Error::Io("Failed to get disk space".to_string()))
        } else {
            Ok(free_bytes_available)
        }
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

    /// Test cross-platform file allocation with Prealloc strategy
    /// Verifies that Prealloc works on Windows, macOS, and Linux
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

    /// Test cross-platform file allocation with Falloc strategy
    /// Verifies that Falloc works on Windows (using set_len fallback) and Unix (using posix_fallocate)
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
        preallocate_file(&path, 1024 * 1024, "trunc", false)
            .await
            .unwrap();

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
        preallocate_file(&path, 1024 * 1024, "none", false)
            .await
            .unwrap();

        // File should not exist
        assert!(!path.exists());
    }

    /// Test allocating a large file (50MB) to verify performance across platforms
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

    /// Test that all three allocation strategies produce same result
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

    /// Verify the workspace default file allocation strategy is `falloc` and
    /// that it parses to `AllocationStrategy::Falloc`.
    #[test]
    fn test_default_allocation_is_falloc() {
        use crate::constants;
        assert_eq!(constants::DEFAULT_FILE_ALLOCATION, "falloc");
        assert_eq!(
            AllocationStrategy::from_str(constants::DEFAULT_FILE_ALLOCATION),
            AllocationStrategy::Falloc
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
        async_zero_fill(&mut adaptor, 5 * 1024 * 1024)
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

    /// Test that secure_falloc option defaults to false in DownloadOptions
    #[test]
    fn test_secure_falloc_default() {
        use crate::request::request_group::DownloadOptions;
        let opts = DownloadOptions::default();
        assert!(!opts.secure_falloc, "secure_falloc should default to false");
    }
}
