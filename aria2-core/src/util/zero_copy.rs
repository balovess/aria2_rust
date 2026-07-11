//! Zero-copy data transfer utilities using Linux `splice(2)`.
//!
//! `splice(2)` moves data between two file descriptors via a kernel pipe
//! buffer without copying the data through user space. For downloads to
//! regular files this enables the pipeline `socket -> pipe -> file`, all
//! inside the kernel, eliminating the user-space `read`/`write` bounce.
//!
//! # Platform Support
//!
//! - **Linux**: Full `splice(2)` support via [`libc`]. The public
//!   [`splice_transfer`] function performs the kernel-mediated transfer.
//! - **Other platforms** (Windows, macOS, ...): [`splice_transfer`] always
//!   returns [`Err`](std::io::Result::Err) with kind
//!   [`Unsupported`](std::io::ErrorKind::Unsupported). Callers must fall back
//!   to a plain `read` + `write` loop. Use [`is_splice_supported`] to gate
//!   the fast path at runtime.
//!
//! # Safety
//!
//! All `splice(2)` invocations are confined to `#[cfg(target_os = "linux")]`
//! code paths and operate on valid, open file descriptors owned by the
//! caller. A RAII [`PipeGuard`] guarantees the intermediate pipe descriptors
//! are closed on every exit path (including early `return` on error and
//! panics).

use std::io;

// `RawFd` only exists in `std::os::fd` on Unix-like platforms. On Windows we
// alias it to `i32` (the same underlying type) purely so the non-Linux
// fallback stub compiles. The stub never performs real I/O, so the alias is
// only present to satisfy the shared function signature.
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(not(unix))]
type RawFd = i32;

/// Chunk size for each `splice` round-trip. 64 KiB matches the default Linux
/// pipe capacity, so a single `splice(fd_in -> pipe)` always fits in the pipe
/// buffer without an `EAGAIN` short read on the pipe side.
#[cfg(target_os = "linux")]
const SPLICE_CHUNK: usize = 64 * 1024;

/// Splice flag: hint the kernel to move pages between buffers instead of
/// copying when possible.
#[cfg(target_os = "linux")]
const SPLICE_F_MOVE: u32 = libc::SPLICE_F_MOVE;

/// Transfer up to `len` bytes from `fd_in` to `fd_out` using `splice(2)`.
///
/// A temporary kernel pipe is created and used as an intermediate buffer:
/// each round performs `splice(fd_in -> pipe)` followed by a fully draining
/// `splice(pipe -> fd_out)`. This keeps the entire byte path inside the
/// kernel — no user-space buffer is ever touched.
///
/// # Arguments
///
/// * `fd_in`  - Source file descriptor (e.g. a TCP socket). Must be seekable
///   or a stream; if `off_in` is `None`, the fd's current offset is used and
///   advanced by the kernel.
/// * `off_in` - Starting offset in the source. `None` uses / advances the fd's
///   own file offset. When `Some`, the kernel updates the offset in place.
/// * `fd_out` - Destination file descriptor (must be a regular file or a
///   pipe — `splice` cannot target a socket with this helper).
/// * `off_out`- Starting offset in the destination, semantics mirror `off_in`.
/// * `len`    - Maximum number of bytes to transfer.
///
/// # Returns
///
/// The number of bytes actually transferred. `0` indicates EOF on the source
/// before any byte was read in the current call.
///
/// # Errors
///
/// Returns [`io::Error`] on any underlying syscall failure. Transient
/// `EINTR` interruptions are retried automatically and never surface to the
/// caller. On non-Linux platforms this always returns
/// [`Unsupported`](std::io::ErrorKind::Unsupported).
///
/// # Examples
///
/// ```no_run
/// # #[cfg(target_os = "linux")] {
/// use std::os::fd::AsRawFd;
/// use aria2_core::util::zero_copy::splice_transfer;
///
/// let src = std::fs::File::open("in.bin")?;
/// let dst = std::fs::OpenOptions::new().write(true).create(true)
///     .truncate(true).open("out.bin")?;
/// let n = splice_transfer(src.as_raw_fd(), None, dst.as_raw_fd(), None, 1 << 20)?;
/// println!("transferred {n} bytes");
/// # }
/// # Ok::<(), std::io::Error>(())
/// ```
#[cfg(target_os = "linux")]
pub fn splice_transfer(
    fd_in: RawFd,
    off_in: Option<i64>,
    fd_out: RawFd,
    off_out: Option<i64>,
    len: usize,
) -> io::Result<usize> {
    // Create the intermediate kernel pipe buffer.
    let mut pipe_fds = [0i32; 2];
    // SAFETY: `pipe_fds` is a valid 2-element array of `c_int`. `pipe(2)`
    // writes two descriptors on success and returns 0.
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // RAII: closes both pipe ends on every exit path, including panics.
    let guard = PipeGuard {
        read: pipe_fds[0],
        write: pipe_fds[1],
    };

    // Mutable copies so the kernel can advance them in place via the pointer
    // we hand to `splice`. `None` keeps using the fd's native file offset.
    let mut in_offset = off_in;
    let mut out_offset = off_out;
    let mut total_transferred = 0usize;

    while total_transferred < len {
        let remaining = len - total_transferred;
        let chunk = remaining.min(SPLICE_CHUNK);

        // Phase 1: source -> pipe. `EINTR` with zero bytes moved is retried.
        let n_read = do_splice(fd_in, in_offset.as_mut(), guard.write, None, chunk)?;
        if n_read == 0 {
            // EOF on the source.
            break;
        }
        let n_read = n_read as usize;

        // Phase 2: drain pipe -> destination completely. A single `splice`
        // into a regular file normally transfers the full amount, but we loop
        // defensively in case of a short write (e.g. ENOSPC mid-way would
        // surface as an error instead).
        let mut drained = 0usize;
        while drained < n_read {
            let to_write = n_read - drained;
            let n_written = do_splice(guard.read, None, fd_out, out_offset.as_mut(), to_write)?;
            if n_written == 0 {
                // Destination accepted no bytes — break to avoid an infinite
                // loop. For regular files this should not happen; surface it.
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "splice: destination accepted zero bytes",
                ));
            }
            drained += n_written as usize;
        }
        total_transferred += drained;
    }

    Ok(total_transferred)
}

/// Non-Linux fallback: always returns [`Unsupported`](std::io::ErrorKind).
///
/// The signature matches the Linux implementation so callers can use the same
/// code unconditionally; they should first check [`is_splice_supported`] and
/// fall back to `read` + `write` when this returns `Err`.
#[cfg(not(target_os = "linux"))]
pub fn splice_transfer(
    _fd_in: RawFd,
    _off_in: Option<i64>,
    _fd_out: RawFd,
    _off_out: Option<i64>,
    _len: usize,
) -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "splice is only supported on Linux",
    ))
}

/// Reports whether `splice(2)` is available on the current platform.
///
/// Callers should gate the fast path with this check and fall back to a
/// user-space `read` + `write` loop when it returns `false`.
pub fn is_splice_supported() -> bool {
    cfg!(target_os = "linux")
}

/// Single `splice(2)` invocation with automatic `EINTR` retry.
///
/// Both offset pointers are optional: `None` passes `NULL` so the kernel uses
/// the fd's native file offset. When `Some(&mut i64)` is supplied, the kernel
/// advances the pointed-to offset in place by the number of bytes moved.
///
/// Returns the number of bytes transferred (`0` indicates EOF on a read side,
/// or end-of-data on a pipe).
#[cfg(target_os = "linux")]
fn do_splice(
    fd_in: RawFd,
    off_in: Option<&mut i64>,
    fd_out: RawFd,
    off_out: Option<&mut i64>,
    len: usize,
) -> io::Result<isize> {
    // `ptr::from_mut` yields a `*mut i64` that stays valid for the borrow's
    // lifetime (the entire call). `None` becomes a NULL pointer.
    let in_ptr = off_in.map(std::ptr::from_mut).unwrap_or(std::ptr::null_mut());
    let out_ptr = off_out
        .map(std::ptr::from_mut)
        .unwrap_or(std::ptr::null_mut());

    loop {
        // SAFETY: `fd_in` and `fd_out` are valid open descriptors supplied by
        // the caller. The offset pointers (when non-NULL) point to mutable
        // locals owned by the caller's stack frame and remain valid for the
        // duration of this call.
        let r = unsafe { libc::splice(fd_in, in_ptr, fd_out, out_ptr, len, SPLICE_F_MOVE) };
        if r < 0 {
            let err = io::Error::last_os_error();
            // Retry on signal interruption. Per POSIX, `splice` returns the
            // byte count if any data was moved before the signal, so `EINTR`
            // only occurs with zero bytes transferred — safe to retry.
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }
        return Ok(r);
    }
}

/// RAII guard closing both ends of a pipe created for a splice transfer.
///
/// Closing both descriptors in `Drop` guarantees no file-descriptor leak,
/// even when the enclosing `splice_transfer` returns early on error or
/// unwinds. Close errors are deliberately ignored: there is no recovery
/// action available during drop, and a failed `close` on a pipe is benign.
#[cfg(target_os = "linux")]
struct PipeGuard {
    read: i32,
    write: i32,
}

#[cfg(target_os = "linux")]
impl Drop for PipeGuard {
    fn drop(&mut self) {
        // SAFETY: both fds were obtained from a successful `pipe(2)` call and
        // are not closed elsewhere. Ignoring the return value is safe for
        // pipe close.
        unsafe {
            libc::close(self.read);
            libc::close(self.write);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_splice_supported() {
        // On Linux this returns true; on every other platform false.
        assert_eq!(is_splice_supported(), cfg!(target_os = "linux"));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn test_splice_unsupported_on_non_linux() {
        // The fallback must refuse gracefully and never panic.
        let result = splice_transfer(0, None, 1, None, 1024);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::Unsupported
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_splice_transfer_between_files() {
        // End-to-end: write known bytes to a source file, splice them into a
        // destination file, then verify the destination content matches.
        use std::os::fd::AsRawFd;

        let dir = tempfile::tempdir().expect("tempdir");
        let src_path = dir.path().join("src.bin");
        let dst_path = dir.path().join("dst.bin");
        let payload = b"splice test data"; // 16 bytes

        std::fs::write(&src_path, payload).expect("write src");

        let src_file = std::fs::File::open(&src_path).expect("open src");
        let dst_file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&dst_path)
            .expect("open dst");

        let n = splice_transfer(
            src_file.as_raw_fd(),
            None,
            dst_file.as_raw_fd(),
            None,
            payload.len(),
        )
        .expect("splice_transfer");
        assert_eq!(n, payload.len());

        // Drop the file handles before re-reading on Windows-like locks.
        drop(src_file);
        drop(dst_file);

        let content = std::fs::read(&dst_path).expect("read dst");
        assert_eq!(content, payload);
    }
}
