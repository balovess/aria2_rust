//! File locking to prevent concurrent aria2 instances from writing to same file.
//!
//! This module provides platform-appropriate file locking mechanisms:
//!
//! - **Unix**: Uses `flock(LOCK_EX | LOCK_NB)` for advisory exclusive locking.
//! - **Windows**: Uses a lock marker file (`.lock` extension) as a simple
//!   exclusive access mechanism.
//! - **Other**: No-op fallback (no locking).
//!
//! # Example
//!
//! ```rust,no_run
//! use aria2_core::filesystem::file_lock::{FileLock, DownloadPathLock};
//! use std::path::Path;
//!
//! // Acquire exclusive lock on a specific file
//! let lock = FileLock::acquire(Path::new("/data/file.zip")).unwrap();
///
/// // ... perform download while holding lock ...
///
/// // Lock is released automatically when `lock` goes out of scope
/// ```
use std::io::Write;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// FileLock -- platform-adaptive exclusive file lock
// ---------------------------------------------------------------------------

/// Exclusive file lock to prevent concurrent aria2 instances from writing to
/// the same file simultaneously.
///
/// Uses platform-appropriate locking mechanism:
/// - On **Unix**: `flock(LOCK_EX | LOCK_NB)` via libc (non-blocking).
/// - On **Windows**: Creates a `.lock` marker file with exclusive semantics.
/// - On **other platforms**: No-op placeholder.
///
/// The lock is automatically released when this struct is dropped.
pub struct FileLock {
    path: PathBuf,
    #[cfg(unix)]
    file: Option<std::fs::File>,
    #[cfg(windows)]
    _handle: Option<()>,
    #[cfg(not(any(unix, windows)))]
    _handle: Option<()>,
}

impl FileLock {
    /// Acquire an exclusive lock on the given file path.
    ///
    /// # Unix Behavior
    ///
    /// Opens/creates the file and calls `flock(fd, LOCK_EX | LOCK_NB)`.
    /// If another process already holds the lock, returns an error immediately
    /// (non-blocking). The lock is **advisory** -- cooperative processes must
    /// all use `flock` for it to be effective. The lock is released when the
    /// returned `FileLock` is dropped (or when [`release`] is called).
    ///
    /// # Windows Behavior
    ///
    /// Creates a sibling `.lock` file in the same directory. This is a simple
    /// marker-based approach; a production implementation would use
    /// `LockFileEx` or a named mutex for true exclusivity.
    ///
    /// # Errors
    ///
    /// Returns `Err` if:
    /// - The file cannot be created/opened (permission denied, disk full, etc.)
    /// - Another process already holds the lock (Unix only)
    pub fn acquire(path: &Path) -> Result<Self, String> {
        #[cfg(unix)]
        {
            Self::acquire_unix(path)
        }
        #[cfg(windows)]
        {
            Self::acquire_windows(path)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Self::acquire_fallback(path)
        }
    }

    /// Unix implementation: flock(LOCK_EX | LOCK_NB).
    #[cfg(unix)]
    fn acquire_unix(path: &Path) -> Result<Self, String> {
        use std::os::unix::io::AsRawFd;

        // Create or open the target file
        let f = std::fs::File::create(path)
            .map_err(|e| format!("Failed to create '{}': {}", path.display(), e))?;

        let fd = f.as_raw_fd();

        // Attempt non-blocking exclusive lock (flock LOCK_EX | LOCK_NB)
        // Returns 0 on success, -1 on error (EAGAIN/EWOULDBLOCK if locked)
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };

        if ret != 0 {
            let err = std::io::Error::last_os_error();
            return Err(format!(
                "Cannot acquire lock on '{}': {} (already in use by another process?)",
                path.display(),
                err
            ));
        }

        tracing::debug!(path = %path.display(), "File lock acquired (Unix flock)");

        Ok(Self {
            path: path.to_path_buf(),
            file: Some(f),
        })
    }

    /// Windows implementation: .lock marker file with exclusive create.
    #[cfg(windows)]
    fn acquire_windows(path: &Path) -> Result<Self, String> {
        // Use a clearly separate lock file name to avoid ambiguity with
        // with_extension() behavior on dotted filenames like ".aria2.lock"
        let lock_path = if let Some(parent) = path.parent() {
            if let Some(stem) = path.file_stem() {
                parent.join(format!("{}.lock", stem.to_string_lossy()))
            } else {
                path.with_extension(".lock")
            }
        } else {
            path.with_extension(".lock")
        };

        // Try to create the lock file exclusively (fails if exists)
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut f) => {
                // Write our PID as lock owner identifier
                let pid = std::process::id();
                let _ = write!(f, "{}", pid);
                // Keep file handle open to maintain lock (dropped = released)
                drop(f);
                Ok(Self {
                    path: path.to_path_buf(),
                    _handle: Some(()),
                })
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    Err(format!(
                        "Cannot acquire lock on '{}': lock file '{}' already exists",
                        path.display(),
                        lock_path.display()
                    ))
                } else {
                    Err(format!(
                        "Failed to create lock file '{}': {}",
                        lock_path.display(),
                        e
                    ))
                }
            }
        }
    }

    /// Fallback for unsupported platforms: no-op.
    #[cfg(not(any(unix, windows)))]
    fn acquire_fallback(path: &Path) -> Result<Self, String> {
        tracing::warn!(
            path = %path.display(),
            "File locking not supported on this platform; proceeding without lock"
        );

        Ok(Self {
            path: path.to_path_buf(),
            _handle: None,
        })
    }

    /// Explicitly release the lock.
    ///
    /// After calling this method, the lock is released and the `FileLock`
    /// should no longer be used. Dropping the `FileLock` also releases the
    /// lock, so explicit release is optional.
    pub fn release(self) {
        #[cfg(unix)]
        {
            if let Some(f) = self.file {
                let fd = f.as_raw_fd();
                let _ = unsafe { libc::flock(fd, libc::LOCK_UN) };
                tracing::debug!(
                    path = %self.path.display(),
                    "File lock released (Unix flock)"
                );
            }
        }
        #[cfg(windows)]
        {
            let lock_path = self.lock_file_path();
            let _ = std::fs::remove_file(&lock_path);
            tracing::debug!(
                path = %self.path.display(),
                lock_file = %lock_path.display(),
                "Lock file removed (Windows)"
            );
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = self; // suppress unused warning
        }
    }

    /// Check whether we currently hold the lock.
    ///
    /// Always returns `true` if [`acquire`] succeeded (the lock is held until
    /// dropped or explicitly released). Returns `false` only if the lock was
    /// never acquired.
    pub fn is_held(&self) -> bool {
        #[cfg(unix)]
        {
            self.file.is_some()
        }
        #[cfg(windows)]
        {
            self._handle.is_some()
        }
        #[cfg(not(any(unix, windows)))]
        {
            false
        }
    }

    /// Return the path that this lock guards.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the actual platform-specific lock file path used for locking.
    ///
    /// - **Unix**: Same as `path()` (the file itself is locked via `flock`).
    /// - **Windows**: The `.lock` marker file (e.g., `foo.lock` for target `foo`).
    pub fn lock_file_path(&self) -> PathBuf {
        #[cfg(unix)]
        {
            self.path.clone()
        }
        #[cfg(windows)]
        {
            // Match the same logic as acquire_windows()
            if let Some(parent) = self.path.parent() {
                if let Some(stem) = self.path.file_stem() {
                    parent.join(format!("{}.lock", stem.to_string_lossy()))
                } else {
                    self.path.with_extension(".lock")
                }
            } else {
                self.path.with_extension(".lock")
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            self.path.clone()
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Release lock on drop (same logic as release but consuming self is not possible in Drop)
        #[cfg(unix)]
        {
            if let Some(ref f) = self.file {
                let fd = f.as_raw_fd();
                let _ = unsafe { libc::flock(fd, libc::LOCK_UN) };
                tracing::debug!(
                    path = %self.path.display(),
                    "File lock auto-released on drop (Unix)"
                );
            }
        }
        #[cfg(windows)]
        {
            let lock_path = self.lock_file_path();
            let _ = std::fs::remove_file(&lock_path);
            tracing::debug!(
                path = %self.path.display(),
                lock_file = %lock_path.display(),
                "Lock file auto-removed on drop (Windows)"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// DownloadPathLock -- manages locks for multi-file downloads
// ---------------------------------------------------------------------------

/// Manages file locks for all files in a download directory.
///
/// Creates a `.aria2.lock` file in the output directory to indicate that an
/// aria2 download is active. This prevents multiple aria2 instances from
/// writing to the same output location simultaneously.
///
/// The lock is held for the lifetime of the `DownloadPathLock` struct and
/// is automatically released when it is dropped.
pub struct DownloadPathLock {
    base_lock: Option<FileLock>,
    dir: PathBuf,
}

impl DownloadPathLock {
    /// Acquire a download lock for the given output directory.
    ///
    /// Creates (or locks) a `.aria2.lock` file inside `output_dir`. If another
    /// aria2 instance already holds a lock on this directory, returns an error.
    ///
    /// # Arguments
    ///
    /// * `output_dir` - The directory where downloaded files will be written.
    ///
    /// # Errors
    ///
    /// Returns `Err` if:
    /// - The directory does not exist and cannot be created.
    /// - Another process holds the lock on `.aria2.lock` in this directory.
    pub fn acquire_for_download(output_dir: &Path) -> Result<Self, String> {
        // Ensure output directory exists
        if !output_dir.exists() {
            std::fs::create_dir_all(output_dir).map_err(|e| {
                format!(
                    "Failed to create output directory '{}': {}",
                    output_dir.display(),
                    e
                )
            })?;
        }

        let lock_path = output_dir.join(".aria2.lock");
        let base = FileLock::acquire(&lock_path)?;

        tracing::info!(
            dir = %output_dir.display(),
            lock = %lock_path.display(),
            "Download path lock acquired"
        );

        Ok(Self {
            base_lock: Some(base),
            dir: output_dir.to_path_buf(),
        })
    }

    /// Release the download lock early (also happens on drop).
    pub fn release(self) {
        if let Some(lock) = self.base_lock {
            lock.release();
            tracing::info!(dir = %self.dir.display(), "Download path lock released");
        }
    }

    /// Return the output directory this lock protects.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Return the underlying FileLock if it exists.
    pub fn file_lock(&self) -> Option<&FileLock> {
        self.base_lock.as_ref()
    }

    /// Check whether the lock is currently held.
    pub fn is_held(&self) -> bool {
        self.base_lock.as_ref().map_or(false, |l| l.is_held())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a unique suffix for test file names to avoid collisions.
    fn unique_suffix() -> String {
        format!(
            "_t{}_p{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or_else(|_| 0, |d| d.as_nanos()),
            std::process::id()
        )
    }

    #[test]
    fn test_file_lock_acquire_release() {
        let tmp_dir = std::env::temp_dir();
        let lock_path = tmp_dir.join(format!("aria2_test_lock{}.tmp", unique_suffix()));

        // Acquire lock should succeed
        let lock = FileLock::acquire(&lock_path).expect("First acquire should succeed");
        assert!(lock.is_held());
        assert_eq!(lock.path(), lock_path);

        // Explicit release
        lock.release();

        // After release, acquiring again should succeed (lock is freed)
        let lock2 = FileLock::acquire(&lock_path).expect("Re-acquire after release should succeed");
        assert!(lock2.is_held());

        // Cleanup
        let _ = std::fs::remove_file(&lock_path);
        #[cfg(windows)]
        let _ = std::fs::remove_file(lock_path.with_extension(".lock"));
    }

    #[test]
    #[cfg(unix)]
    fn test_file_lock_double_acquire_fails() {
        let tmp_dir = std::env::temp_dir();
        let lock_path = tmp_dir.join(format!("aria2_test_double_lock{}.tmp", unique_suffix()));

        // First acquire succeeds
        let lock1 = FileLock::acquire(&lock_path).expect("First acquire should succeed");

        // Second acquire on same path should FAIL (non-blocking flock returns EAGAIN)
        let result = FileLock::acquire(&lock_path);
        assert!(
            result.is_err(),
            "Second acquire on same path should fail, got: {:?}",
            result
        );

        // Drop first lock
        drop(lock1);

        // Now acquire should succeed again
        let lock3 = FileLock::acquire(&lock_path).expect("Acquire after drop should succeed");
        assert!(lock3.is_held());

        // Cleanup
        let _ = std::fs::remove_file(&lock_path);
    }

    #[test]
    fn test_download_path_lock_creates_marker() {
        let tmp_dir = std::env::temp_dir();
        let test_dir = tmp_dir.join(format!("aria2_test_dl_lock{}", unique_suffix()));

        // Ensure clean state
        let _ = std::fs::remove_dir_all(&test_dir);

        // Acquire download path lock
        let dl_lock =
            DownloadPathLock::acquire_for_download(&test_dir).expect("Acquire should succeed");

        // Verify lock marker file exists while lock is held (use platform-aware path)
        let lock_file = dl_lock.base_lock.as_ref().unwrap().lock_file_path();
        assert!(
            lock_file.exists(),
            "lock marker file should exist while lock is held"
        );
        assert!(dl_lock.is_held());
        assert_eq!(dl_lock.dir(), test_dir);

        // Verify directory was created
        assert!(test_dir.exists());

        // Release lock
        dl_lock.release();

        // After release, the lock file should be cleaned up (platform dependent)
        #[cfg(windows)]
        assert!(
            !lock_file.exists(),
            ".lock file should be removed after release on Windows"
        );

        // Cleanup
        let _ = std::fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_download_path_lock_reacquire_after_drop() {
        let tmp_dir = std::env::temp_dir();
        let test_dir = tmp_dir.join(format!("aria2_test_reacquire{}", unique_suffix()));

        // Clean up
        let _ = std::fs::remove_dir_all(&test_dir);

        {
            // Scope 1: acquire and hold
            let _lock1 =
                DownloadPathLock::acquire_for_download(&test_dir).expect("First acquire OK");
            let lock_path = _lock1.file_lock().unwrap().lock_file_path();
            assert!(lock_path.exists());
        }
        // _lock1 dropped here -> lock released

        {
            // Scope 2: re-acquire should succeed
            let _lock2 = DownloadPathLock::acquire_for_download(&test_dir)
                .expect("Re-acquire after drop OK");
            assert!(_lock2.is_held());
        }

        // Cleanup
        let _ = std::fs::remove_dir_all(&test_dir);
    }
}
