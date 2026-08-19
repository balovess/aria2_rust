//! Positioned disk writer using OS-native `pwrite`/`seek_write` for concurrent
//! writes to non-overlapping offsets without a global async mutex.
//!
//! Platform support:
//! - Unix: [`std::os::unix::fs::FileExt::write_at`] (wraps `pwrite(2)`)
//! - Windows: [`std::os::windows::fs::FileExt::seek_write`]
//!
//! # Concurrency model
//!
//! The underlying file handle is shared through `Arc`, while each potentially
//! blocking `pwrite`/`seek_write` call runs on Tokio's blocking pool. This is
//! fundamentally different from the legacy `Arc<tokio::sync::Mutex<...>>`
//! design which held the lock across async await points and serialized writes.
//!
//! Here the lock is held only for the synchronous syscall (microseconds),
//! never across `.await` points. When multiple [`PositionedDiskWriter`]
//! instances reference the same file path (each opening its own file
//! descriptor), non-overlapping writes execute concurrently at the OS level
//! because `pwrite` is atomic and offset-based — it does not mutate the
//! shared file cursor.

mod platform_io;
mod writer;

#[cfg(all(target_os = "linux", feature = "io_uring"))]
mod io_uring;

#[cfg(test)]
mod tests;

#[cfg(all(test, target_os = "linux", feature = "io_uring"))]
mod io_uring_tests;

// Re-export the primary public types and factory function.
pub use writer::PositionedDiskWriter;

#[cfg(all(target_os = "linux", feature = "io_uring"))]
pub use io_uring::IoUringDiskWriter;

use std::path::Path;

use crate::filesystem::disk_writer::SeekableDiskWriter;

/// Create the best available positioned writer for the current platform.
///
/// On Linux with the `io_uring` feature enabled, returns an [`IoUringDiskWriter`]
/// that uses the io_uring syscall interface for async positioned I/O. On all
/// other platforms (or without the feature), returns a [`PositionedDiskWriter`]
/// that uses synchronous `pwrite`/`seek_write`.
///
/// # Runtime requirement (io_uring)
///
/// When the `io_uring` feature is enabled on Linux, the returned writer MUST be
/// driven from within a `tokio_uring` runtime context (e.g. inside
/// `tokio_uring::start`). Using it inside a regular `tokio` runtime will panic
/// because `tokio_uring::fs` operations require the io_uring reactor.
///
/// This factory is intentionally NOT wired into the default download pipeline.
/// The main pipeline uses [`PositionedDiskWriter`] directly via `CachedDiskWriter`.
pub fn create_positioned_writer(
    path: &Path,
    total_size: Option<u64>,
) -> Box<dyn SeekableDiskWriter> {
    #[cfg(all(target_os = "linux", feature = "io_uring"))]
    {
        Box::new(IoUringDiskWriter::new(path, total_size))
    }
    #[cfg(not(all(target_os = "linux", feature = "io_uring")))]
    {
        Box::new(PositionedDiskWriter::new(path, total_size))
    }
}
