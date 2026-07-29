use crate::error::{Aria2Error, Result};
use crate::filesystem::disk_adaptor::DiskAdaptor;
use std::path::Path;

/// Preallocate file space using truncation (set_len).
/// This is the simplest allocation method that works on all platforms:
/// - Unix: Uses ftruncate via set_len
/// - Windows: Uses SetEndOfFile via set_len
/// - macOS: Uses set_len
///
/// Note: This method does not guarantee contiguous disk space allocation,
/// but it ensures the file has the specified size.
pub(crate) async fn preallocate<D: DiskAdaptor>(adaptor: &mut D, length: u64) -> Result<()> {
    adaptor.truncate(length).await
}

/// Truncate file to the specified length.
/// Works identically on all platforms using set_len:
/// - Unix: ftruncate system call
/// - Windows: SetEndOfFile API
/// - macOS: ftruncate
pub(crate) async fn truncate<D: DiskAdaptor>(adaptor: &mut D, length: u64) -> Result<()> {
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
pub(crate) async fn async_zero_fill<D: DiskAdaptor>(adaptor: &mut D, length: u64) -> Result<()> {
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

/// Get available disk space at the filesystem containing `path`.
///
/// Platform-specific implementations:
/// - **Linux**: Uses `statvfs64`
/// - **macOS / other Unix**: Uses `statvfs`
/// - **Windows**: Uses `GetDiskFreeSpaceExW`
/// - **Other**: Returns `u64::MAX` as a sentinel
pub async fn get_available_space(path: &Path) -> Result<u64> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));

    #[cfg(target_os = "linux")]
    {
        let _metadata = tokio::fs::metadata(parent)
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;

        // SAFETY: statvfs64 is a standard POSIX syscall. stat is a
        // zeroed statvfs64 struct on the stack. The path pointer is
        // valid UTF-8 (falling back to "." for current directory).
        // The pointer remains valid for the duration of the syscall.
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
        // On macOS and other Unix systems, use statvfs (not statvfs64).
        // macOS statvfs already handles large files.
        let _metadata = tokio::fs::metadata(parent)
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;

        // SAFETY: statvfs is a standard POSIX syscall. stat is a
        // zeroed statvfs struct on the stack. The path pointer is valid
        // UTF-8 (falling back to "." for current directory).
        // The pointer remains valid for the duration of the syscall.
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
            Err(_) => std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf()),
        };

        // Convert path to wide string with null terminator
        let mut wide_path: Vec<u16> = abs_path.as_os_str().encode_wide().collect();
        wide_path.push(0);

        let mut free_bytes_available: u64 = 0;
        // SAFETY: wide_path is a valid null-terminated wide string.
        // &mut free_bytes_available is a valid pointer. Null output
        // parameters are safe (we only need free bytes available).
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
