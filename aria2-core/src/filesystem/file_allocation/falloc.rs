use crate::error::Result;
use crate::filesystem::disk_adaptor::DiskAdaptor;

#[cfg(unix)]
use crate::error::Aria2Error;

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
            use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

            // Keep the descriptor alive if the allocation future is cancelled
            // while the blocking syscall is still running.
            let duplicated_fd = unsafe { libc::dup(fd) };
            if duplicated_fd < 0 {
                return Err(Aria2Error::Io(std::io::Error::last_os_error().to_string()));
            }
            // SAFETY: `duplicated_fd` is the successful result of `dup` and is
            // owned exclusively by this `OwnedFd`.
            let owned_fd = unsafe { OwnedFd::from_raw_fd(duplicated_fd) };
            let syscall_result = tokio::task::spawn_blocking(move || {
                // `fallocate` may wait for filesystem metadata, block
                // allocation, or CoW extent work. Keep it off the reactor.
                let ret = unsafe {
                    libc::fallocate(
                        owned_fd.as_raw_fd(),
                        0 as libc::c_int,
                        0,
                        length as libc::off_t,
                    )
                };
                if ret == 0 {
                    Ok(())
                } else {
                    // SAFETY: errno is read immediately after fallocate on
                    // the same blocking worker thread.
                    Err(unsafe { *libc::__errno_location() })
                }
            })
            .await
            .map_err(|error| Aria2Error::Io(format!("fallocate task failed: {error}")))?;
            if syscall_result.is_ok() {
                // Success: kernel allocates zeroed blocks; secure is a no-op.
                return Ok(());
            }
            let errno = syscall_result.expect_err("failed fallocate must carry errno");
            if errno == libc::EOPNOTSUPP {
                tracing::warn!(
                    length,
                    "fallocate(2) not supported by filesystem; \
                     falling back to async zero-fill"
                );
                // Preserve an existing partial download and clear only the
                // newly extended region when falling back to zero-fill.
                let existing_length = adaptor.size().await?.min(length);
                adaptor.truncate(length).await?;
                return strategies::async_zero_fill_from(adaptor, existing_length, length).await;
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
                    let existing_length = adaptor.size().await?.min(length);
                    adaptor.truncate(length).await?;
                    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

                    // Keep the descriptor alive if the allocation future is
                    // cancelled while F_PREALLOCATE is still running.
                    let duplicated_fd = unsafe { libc::dup(fd) };
                    if duplicated_fd < 0 {
                        return Err(Aria2Error::Io(std::io::Error::last_os_error().to_string()));
                    }
                    // SAFETY: `duplicated_fd` is the successful result of
                    // `dup` and is owned exclusively by this `OwnedFd`.
                    let owned_fd = unsafe { OwnedFd::from_raw_fd(duplicated_fd) };
                    let fcntl_result = tokio::task::spawn_blocking(move || {
                        // `F_PREALLOCATE` may synchronously allocate extents.
                        let fstore = libc::fstore_t {
                            fst_flags: libc::F_ALLOCATEALL as libc::c_uint,
                            fst_posmode: libc::F_PEOFPOSMODE,
                            fst_offset: 0,
                            fst_length: length as libc::off_t,
                            fst_bytesalloc: 0,
                        };
                        // SAFETY: fd is a valid file descriptor and fstore
                        // remains live for the duration of the syscall.
                        let ret = unsafe {
                            libc::fcntl(owned_fd.as_raw_fd(), libc::F_PREALLOCATE, &fstore)
                        };
                        if ret == 0 {
                            Ok(())
                        } else {
                            Err(std::io::Error::last_os_error())
                        }
                    })
                    .await
                    .map_err(|error| {
                        Aria2Error::Io(format!("F_PREALLOCATE task failed: {error}"))
                    })?;
                    if let Err(error) = fcntl_result {
                        // Filesystem may not support F_PREALLOCATE; the file is
                        // already sized above, so it remains sparse but correct.
                        tracing::warn!(
                            length,
                            error = %error,
                            "F_PREALLOCATE failed on macOS, file remains sparse"
                        );
                        return Ok(());
                    }
                    // F_PREALLOCATE succeeded but does NOT zero-fill the
                    // allocated blocks. Zero-fill when secure is requested,
                    // otherwise emit a one-time warning about the trade-off.
                    if secure {
                        strategies::async_zero_fill_from(adaptor, existing_length, length).await
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
