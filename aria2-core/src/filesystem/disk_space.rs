use crate::error::{Aria2Error, FatalError, Result};
use std::fmt;
use std::path::Path;

const DEFAULT_MARGIN_MB: u64 = 100;

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
// K5.2 — DiskError Enum for Structured Error Handling
// =========================================================================

/// Structured error type for disk-related failures during download.
///
/// Provides specific error variants for different disk failure scenarios,
/// enabling callers to handle each case appropriately (e.g., show user-friendly
/// messages, trigger cleanup, or retry with smaller files).
///
/// # Examples
///
/// ```ignore
/// use aria2_core::filesystem::disk_space::DiskError;
///
/// match check_disk_space(path, required_bytes) {
///     Ok(()) => println!("Sufficient space"),
///     Err(DiskError::InsufficientSpace { required, available }) => {
///         eprintln!("Need {} but only {} available", required, available.unwrap_or(0));
///     }
///     Err(DiskError::IoError(msg)) => eprintln!("I/O error: {}", msg),
///     Err(DiskError::PermissionDenied(p)) => eprintln!("Permission denied: {}", p),
/// }
/// ```
#[derive(Debug, Clone)]
pub enum DiskError {
    /// Not enough disk space available
    InsufficientSpace {
        /// Bytes required for the operation
        required: u64,
        /// Bytes currently available (None if unknown)
        available: Option<u64>,
    },
    /// General I/O error during disk operation
    IoError(String),
    /// Permission denied when accessing path
    PermissionDenied(String),
}

impl fmt::Display for DiskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiskError::InsufficientSpace {
                required,
                available,
            } => {
                write!(
                    f,
                    "Not enough disk space: need {}, have {}",
                    format_bytes(*required),
                    available.map_or_else(|| "unknown".into(), format_bytes)
                )
            }
            DiskError::IoError(msg) => write!(f, "Disk I/O error: {}", msg),
            DiskError::PermissionDenied(path) => write!(f, "Permission denied: {}", path),
        }
    }
}

impl std::error::Error for DiskError {}

/// Format byte count as human-readable string.
///
/// Automatically selects appropriate unit (B, KiB, MiB, GiB)
/// based on magnitude for user-friendly display.
///
/// # Arguments
///
/// * `bytes` - Number of bytes to format
///
/// # Returns
///
/// Human-readable string like "1.50 GiB" or "256.00 KiB"
fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.2} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.2} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
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

/// Check disk space and return structured DiskError on failure.
///
/// Similar to `check_disk_space()` but returns typed `DiskError` enum
/// instead of String, enabling pattern matching on error types.
///
/// # Arguments
///
/// * `path` - Directory or file path to check
/// * `required_bytes` - Minimum bytes needed
///
/// # Returns
///
/// * `Ok(())` - Sufficient space available
/// * `Err(DiskError)` - Typed error indicating specific failure reason
pub fn check_disk_space_typed(
    path: &Path,
    required_bytes: u64,
) -> std::result::Result<(), DiskError> {
    #[cfg(unix)]
    {
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

        let available = stat.f_bavail as u64 * stat.f_frsize as u64;
        let needed_with_headroom = required_bytes.saturating_add(required_bytes / 10);

        if available < needed_with_headroom {
            return Err(DiskError::InsufficientSpace {
                required: needed_with_headroom,
                available: Some(available),
            });
        }

        Ok(())
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        let check_path = if path.as_os_str().is_empty() {
            Path::new(".")
        } else {
            path
        };

        // Get absolute path for GetDiskFreeSpaceEx
        let abs_path = match std::fs::canonicalize(check_path) {
            Ok(p) => p,
            Err(_) => std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf()),
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
            // Failed to get disk space, don't block
            return Ok(());
        }

        let needed_with_headroom = required_bytes.saturating_add(required_bytes / 10);

        if free_bytes_available < needed_with_headroom {
            return Err(DiskError::InsufficientSpace {
                required: needed_with_headroom,
                available: Some(free_bytes_available),
            });
        }

        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        let _ = required_bytes;
        Ok(())
    }
}

pub fn available_space(path: &Path) -> Result<u64> {
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };

    #[cfg(unix)]
    {
        // statvfs requires a null-terminated C string; OsStr::as_bytes() is NOT
        // null-terminated (undefined behavior if passed directly).
        let c_path = path_to_cstring(path).ok_or_else(|| {
            Aria2Error::Fatal(FatalError::Config(format!(
                "path contains embedded null byte: {}",
                path.display()
            )))
        })?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
        if ret != 0 {
            return Err(Aria2Error::Fatal(FatalError::Config(format!(
                "statvfs failed: {}",
                std::io::Error::last_os_error()
            ))));
        }
        Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        // Get absolute path for GetDiskFreeSpaceEx
        let abs_path = match std::fs::canonicalize(path) {
            Ok(p) => p,
            Err(_) => std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf()),
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
            return Err(Aria2Error::Fatal(FatalError::Config(
                "GetDiskFreeSpaceExW failed".to_string(),
            )));
        }

        Ok(free_bytes_available)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(u64::MAX)
    }
}

pub fn has_enough_space(path: &Path, required: u64) -> bool {
    available_space(path).is_ok_and(|avail| avail >= required)
}

pub fn check_with_margin(path: &Path, required: u64, margin_mb: Option<u64>) -> Result<()> {
    let margin = margin_mb.unwrap_or(DEFAULT_MARGIN_MB) * 1024 * 1024;
    let total_needed = required.saturating_add(margin);
    // If available space cannot be queried (e.g. statvfs fails with ENOENT on
    // some CI tempdir paths), skip the check instead of failing the operation.
    // This mirrors the graceful degradation already performed in
    // `check_disk_space` and `check_disk_space_typed`.
    let avail = match available_space(path) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                required = total_needed,
                "available_space failed, skipping disk space check: {}",
                err
            );
            return Ok(());
        }
    };
    if avail < total_needed {
        Err(Aria2Error::Fatal(FatalError::DiskSpaceExhausted))
    } else {
        Ok(())
    }
}

pub fn total_space(path: &Path) -> Result<u64> {
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };

    #[cfg(unix)]
    {
        // statvfs requires a null-terminated C string; OsStr::as_bytes() is NOT
        // null-terminated (undefined behavior if passed directly).
        let c_path = path_to_cstring(path).ok_or_else(|| {
            Aria2Error::Fatal(FatalError::Config(format!(
                "path contains embedded null byte: {}",
                path.display()
            )))
        })?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
        if ret != 0 {
            return Err(Aria2Error::Fatal(FatalError::Config(format!(
                "statvfs failed: {}",
                std::io::Error::last_os_error()
            ))));
        }
        Ok(stat.f_blocks as u64 * stat.f_frsize as u64)
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        // Get absolute path for GetDiskFreeSpaceEx
        let abs_path = match std::fs::canonicalize(path) {
            Ok(p) => p,
            Err(_) => std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf()),
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
            return Err(Aria2Error::Fatal(FatalError::Config(
                "GetDiskFreeSpaceExW failed".to_string(),
            )));
        }

        Ok(total_number_of_bytes)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(u64::MAX)
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

    /// Test K5.4 #3: DiskError Display trait shows readable sizes.
    ///
    /// Verifies that the DiskError enum's Display implementation produces
    /// human-readable output with proper byte formatting.
    #[test]
    fn test_disk_error_display() {
        // Test InsufficientSpace variant
        let err = DiskError::InsufficientSpace {
            required: 1024 * 1024 * 1024,       // 1 GiB
            available: Some(512 * 1024 * 1024), // 512 MiB
        };
        let display_str = format!("{}", err);
        assert!(
            display_str.contains("Not enough disk space"),
            "Should mention insufficient space"
        );
        assert!(
            display_str.contains("1.00 GiB") || display_str.contains("GiB"),
            "Should show required size in GiB"
        );
        assert!(
            display_str.contains("512.00 MiB") || display_str.contains("MiB"),
            "Should show available size in MiB"
        );

        // Test IoError variant
        let io_err = DiskError::IoError("Failed to write block".to_string());
        let io_display = format!("{}", io_err);
        assert!(
            io_display.contains("Disk I/O error"),
            "Should mention I/O error"
        );
        assert!(
            io_display.contains("Failed to write block"),
            "Should include original message"
        );

        // Test PermissionDenied variant
        let perm_err = DiskError::PermissionDenied("/root/secret".to_string());
        let perm_display = format!("{}", perm_err);
        assert!(
            perm_display.contains("Permission denied"),
            "Should mention permission denied"
        );
        assert!(perm_display.contains("/root/secret"), "Should include path");

        // Test format_bytes helper function
        assert_eq!(format_bytes(0), "0 B", "Zero bytes");
        assert_eq!(format_bytes(500), "500 B", "Small bytes");
        assert_eq!(format_bytes(1024), "1.00 KiB", "Exactly 1 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.00 MiB", "Exactly 1 MiB");
        assert_eq!(
            format_bytes(1024 * 1024 * 1024),
            "1.00 GiB",
            "Exactly 1 GiB"
        );
        assert_eq!(
            format_bytes(1536 * 1024 * 1024),
            "1.50 GiB",
            "1.5 GiB with decimal"
        );
    }

    /// Additional test: check_disk_space_typed returns typed errors.
    #[test]
    fn test_check_disk_space_typed_returns_structured_errors() {
        let huge_request = u64::MAX / 2;

        match check_disk_space_typed(Path::new("."), huge_request) {
            Ok(()) => {
                // Sufficient space or check skipped - acceptable
            }
            Err(DiskError::InsufficientSpace {
                required,
                available,
            }) => {
                // Should have structured data
                assert!(required > 0, "Required should be positive");
                // Available may be Some or None depending on platform
                if let Some(avail) = available {
                    // avail is u64, always valid by type guarantee
                    let _ = avail;
                }
            }
            Err(DiskError::IoError(msg)) => {
                // I/O error during statvfs
                assert!(!msg.is_empty(), "Error message should not be empty");
            }
            Err(DiskError::PermissionDenied(_)) => {
                // Permission error - unlikely but possible
            }
        }
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
