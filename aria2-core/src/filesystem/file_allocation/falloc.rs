use crate::error::Result;
use crate::filesystem::disk_adaptor::DiskAdaptor;

#[cfg(unix)]
use super::strategies;

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
/// - **Windows**: Delegates to `windows::fallocate_windows` which attempts
///   `SetFileValidData` to extend the valid data length and force allocation.
///   See the `windows` module for details.
/// - **Other Unix (BSD, etc.)**: No portable preallocate syscall; uses `set_len`.
#[cfg_attr(target_os = "linux", allow(unused_variables))]
pub(crate) async fn fallocate<D: DiskAdaptor>(
    adaptor: &mut D,
    length: u64,
    secure: bool,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if let Some(fd) = adaptor.unix_raw_fd() {
            // Validate length fits in off_t (i64 on 64-bit, i32 on 32-bit).
            if length > i64::MAX as u64 {
                return Err(Aria2Error::Io(
                    "fallocate length exceeds off_t range".into(),
                ));
            }
            // Use raw fallocate(2) to detect EOPNOTSUPP explicitly.
            // posix_fallocate64 would silently fall back to a blocking
            // zero-fill loop inside libc when the kernel returns EOPNOTSUPP,
            // which would stall the async runtime. By calling fallocate(2)
            // directly we can fall back to our own cooperative async_zero_fill.
            //
            // FALLOC_FL_NONE (0) requests default behavior: allocate space
            // and zero-fill it at the filesystem block level.
            // SAFETY: fd is a valid open file descriptor obtained from
            // adaptor.unix_raw_fd() (checked above). fallocate(2) is a
            // standard Linux syscall. The mode 0 (FALLOC_FL_NONE) requests
            // default allocate-and-zero-fill behavior. length is cast to
            // off_t which is i64 on 64-bit Linux; u64 fits in i64 for
            // practical file sizes (< 2^63 bytes).
            let ret = unsafe { libc::fallocate(fd, 0 as libc::c_int, 0, length as libc::off_t) };
            if ret == 0 {
                // Success: kernel allocates zeroed blocks; secure is a no-op.
                return Ok(());
            }
            // SAFETY: __errno_location() returns a pointer to the
            // thread-local errno variable. Dereferencing is safe in a
            // single-threaded context immediately after the fallocate call.
            let errno = unsafe { *libc::__errno_location() };
            if errno == libc::EOPNOTSUPP {
                tracing::warn!(
                    length,
                    "fallocate(2) not supported by filesystem; \
                     falling back to async zero-fill"
                );
                // Size the file first so writes land at correct offsets.
                adaptor.truncate(length).await?;
                return strategies::async_zero_fill(adaptor, length).await;
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
                    // Validate length fits in off_t
                    if length > i64::MAX as u64 {
                        return Err(Aria2Error::Io(
                            "F_PREALLOCATE length exceeds off_t range".into(),
                        ));
                    }
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
                    // SAFETY: fd is a valid open file descriptor.
                    // F_PREALLOCATE is a valid fcntl command on macOS.
                    // &fstore is a valid pointer to a fstore_t struct
                    // on the stack that outlives this call.
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
                        strategies::async_zero_fill(adaptor, length).await
                    } else {
                        super::SECURE_FALLOC_WARN_ONCE.call_once(|| {
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
        super::windows::fallocate_windows(adaptor, length, secure).await
    }
}
