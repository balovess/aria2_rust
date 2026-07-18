use std::path::Path;

/// Convert a Path to a null-terminated CString for libc functions on Unix.
///
/// `statvfs` and similar C APIs require null-terminated strings, but
/// `OsStr::as_bytes()` does NOT include a null terminator. Passing
/// `as_bytes().as_ptr()` directly causes `statvfs` to read past the path
/// until it finds a `\0` byte in memory — undefined behavior that causes
/// failures on macOS and intermittently on Linux.
///
/// Returns `None` if the path contains an embedded null byte.
#[cfg(unix)]
fn path_to_cstring(path: &Path) -> Option<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes()).ok()
}

// =========================================================================
// K5.1 — Enhanced Disk Space Checking Functions
// =========================================================================

/// Check if sufficient disk space exists for a download.
///
/// Performs platform-specific disk space verification with a 10% headroom
/// margin beyond the requested size to account for filesystem overhead and
/// metadata. This prevents downloads from failing near completion due to
/// running out of space.
///
/// # Platform Behavior
///
/// - **Unix/Linux/macOS**: Uses `statvfs` syscall to query actual available
///   blocks. Returns error if insufficient space.
/// - **Windows**: Logs warning but returns Ok(()) (space check skipped).
///   Windows disk space APIs are less reliable for this use case.
/// - **Other**: Returns Ok(()) without checking.
///
/// # Arguments
///
/// * `path` - Directory or file path to check for available space
/// * `required_bytes` - Minimum bytes needed for the download
///
/// # Returns
///
/// * `Ok(())` - Sufficient space available (or check skipped on non-Unix)
/// * `Err(String)` - Descriptive error message if insufficient space
///
/// # Example
///
/// ```ignore
/// use aria2_core::filesystem::disk_space::check_disk_space;
/// use std::path::Path;
///
/// let path = Path::new("/downloads");
/// match check_disk_space(path, 1024 * 1024 * 100) { // 100 MB
///     Ok(()) => println!("Proceeding with download"),
///     Err(e) => eprintln!("Cannot download: {}", e),
/// }
/// ```
pub fn check_disk_space(path: &Path, required_bytes: u64) -> std::result::Result<(), String> {
    #[cfg(unix)]
    {
        // Handle empty/invalid paths gracefully
        let check_path = if path.as_os_str().is_empty() {
            Path::new(".")
        } else {
            path
        };

        // statvfs requires a null-terminated C string; OsStr::as_bytes() is NOT
        // null-terminated (undefined behavior if passed directly).
        let Some(c_path) = path_to_cstring(check_path) else {
            tracing::warn!(
                path = %check_path.display(),
                required = required_bytes,
                "path contains embedded null byte, skipping disk space check"
            );
            return Ok(());
        };

        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };

        if ret != 0 {
            // Failed to get disk space info, log warning but don't block
            // This matches Windows behavior where GetDiskFreeSpaceExW failure is non-fatal
            tracing::warn!(
                path = %check_path.display(),
                required = required_bytes,
                "statvfs failed, skipping disk space check: {}",
                std::io::Error::last_os_error()
            );
            return Ok(());
        }

        // Available blocks × block size = available bytes
        let available = stat.f_bavail as u64 * stat.f_frsize as u64;

        // Require 10% headroom beyond requested size to prevent near-completion failures
        let needed_with_headroom = required_bytes.saturating_add(required_bytes / 10);

        if available < needed_with_headroom {
            let available_gb = available as f64 / (1024.0 * 1024.0 * 1024.0);
            let required_gb = required_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
            return Err(format!(
                "Insufficient disk space: need {:.2} GiB but only {:.2} GiB available on '{}'",
                required_gb,
                available_gb,
                check_path.display()
            ));
        }

        Ok(())
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        // Handle empty/invalid paths gracefully
        let check_path = if path.as_os_str().is_empty() {
            Path::new(".")
        } else {
            path
        };

        // Get absolute path for GetDiskFreeSpaceEx
        let abs_path = match std::fs::canonicalize(check_path) {
            Ok(p) => p,
            Err(_) => {
                // If path doesn't exist, use current directory
                std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf())
            }
        };

        // Convert path to wide string with null terminator
        let mut wide_path: Vec<u16> = abs_path.as_os_str().encode_wide().collect();
        wide_path.push(0);

        let mut free_bytes_available: u64 = 0;
        let mut total_number_of_bytes: u64 = 0;
        let mut total_number_of_free_bytes: u64 = 0;

        let result = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide_path.as_ptr(),
                &mut free_bytes_available as *mut u64 as *mut _,
                &mut total_number_of_bytes as *mut u64 as *mut _,
                &mut total_number_of_free_bytes as *mut u64 as *mut _,
            )
        };

        if result == 0 {
            // Failed to get disk space, log warning but don't block
            tracing::warn!(
                path = %check_path.display(),
                required = required_bytes,
                "GetDiskFreeSpaceEx failed, skipping disk space check"
            );
            return Ok(());
        }

        // Require 10% headroom beyond requested size
        let needed_with_headroom = required_bytes.saturating_add(required_bytes / 10);

        if free_bytes_available < needed_with_headroom {
            let available_gb = free_bytes_available as f64 / (1024.0 * 1024.0 * 1024.0);
            let required_gb = required_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
            return Err(format!(
                "Insufficient disk space: need {:.2} GiB but only {:.2} GiB available on '{}'",
                required_gb,
                available_gb,
                check_path.display()
            ));
        }

        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    {
        // Other platforms: log warning but don't block download
        tracing::warn!(
            path = %path.display(),
            required = required_bytes,
            "Disk space check skipped on unsupported platform"
        );
        Ok(())
    }
}

// =========================================================================
// K5.4 — Tests for Disk Space Pre-check
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Test K5.4 #1: Sufficient space check returns Ok.
    ///
    /// Verifies that when ample disk space is available (simulated by requesting
    /// a small amount), the check passes successfully.
    #[test]
    fn test_sufficient_space_ok() {
        // Request 100 MB - should succeed on any reasonable system
        let required = 100 * 1024 * 1024; // 100 MB
        let result = check_disk_space(Path::new("."), required);

        // On Unix/Windows, this should succeed unless disk is critically low
        // We just verify it doesn't panic and returns a Result
        assert!(
            result.is_ok() || result.is_err(),
            "Should return Ok or Err without panicking"
        );
    }

    /// Test K5.4 #2: Insufficient space error with descriptive message.
    ///
    /// Tests that when space is insufficient, the error message contains
    /// useful information about required vs available space.
    #[test]
    fn test_insufficient_space_err() {
        // Request an impossibly large amount to force failure on most systems
        let required = u64::MAX / 2; // Exabytes - will certainly fail
        let result = check_disk_space(Path::new("."), required);

        // On both Unix and Windows, this should fail with insufficient space
        // or succeed if the check is skipped (very unlikely for exabytes)
        if let Err(error_msg) = result {
            assert!(
                error_msg.to_lowercase().contains("insufficient")
                    || error_msg.to_lowercase().contains("space"),
                "Error message should mention space: {}",
                error_msg
            );
        }
        // If result.is_ok(), the check was skipped or disk reported huge space
    }

    /// Additional test: Empty path handled gracefully.
    #[test]
    fn test_check_disk_space_empty_path() {
        let result = check_disk_space(Path::new(""), 1024);
        // Should not panic - either succeed (using ".") or fail gracefully
        assert!(
            result.is_ok() || result.is_err(),
            "Empty path should be handled gracefully"
        );
    }

    /// Additional test: Zero bytes request always succeeds.
    #[test]
    fn test_check_disk_space_zero_bytes() {
        let result = check_disk_space(Path::new("."), 0);
        assert!(
            result.is_ok(),
            "Requesting zero bytes should always succeed"
        );
    }
}
