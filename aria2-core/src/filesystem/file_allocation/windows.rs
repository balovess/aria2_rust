use crate::error::{Aria2Error, Result};
use crate::filesystem::disk_adaptor::DiskAdaptor;

use super::strategies;

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
pub(crate) fn try_enable_volume_privilege() -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, SE_MANAGE_VOLUME_NAME,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token_handle = std::ptr::null_mut();
    // SAFETY: GetCurrentProcess() returns a pseudo-handle that is
    // always valid. &mut token_handle is a valid pointer to a HANDLE
    // on the stack. The call does not race with any other thread.
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

    // SAFETY: LUID is a POD struct (two DWORDs) that is valid when zeroed.
    let mut luid: windows_sys::Win32::Foundation::LUID = unsafe { std::mem::zeroed() };
    // SAFETY: First arg NULL means local system. SE_MANAGE_VOLUME_NAME
    // is a valid well-known privilege name. &mut luid is a valid pointer.
    if unsafe { LookupPrivilegeValueW(std::ptr::null(), SE_MANAGE_VOLUME_NAME, &mut luid) } == 0 {
        // SAFETY: token_handle is a valid handle from OpenProcessToken.
        unsafe {
            CloseHandle(token_handle);
        }
        return false;
    }

    // SAFETY: TOKEN_PRIVILEGES is a POD struct valid when zeroed.
    // We immediately set PrivilegeCount and Privileges[0] after.
    let mut tp: TOKEN_PRIVILEGES = unsafe { std::mem::zeroed() };
    tp.PrivilegeCount = 1;
    tp.Privileges[0].Luid = luid;
    tp.Privileges[0].Attributes = 2; // SE_PRIVILEGE_ENABLED

    // SAFETY: token_handle is a valid token handle from OpenProcessToken.
    // &tp is a valid TOKEN_PRIVILEGES struct. Null output params are safe.
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
    // SAFETY: token_handle is a valid handle from OpenProcessToken.
    unsafe {
        CloseHandle(token_handle);
    }
    ret != 0
}

/// Windows-specific fallocate implementation.
///
/// Attempts `SetFileValidData` to extend the valid data length and force
/// allocation. `SetFileValidData` does NOT zero-fill, so when `secure == true`
/// we additionally run `async_zero_fill`. When `secure == false`, a one-time
/// warning is emitted. Requires `SE_MANAGE_VOLUME_PRIVILEGE`; if the privilege
/// is not held (or the call fails for any other reason), it falls back to the
/// sparse file produced by `set_len` (SetEndOfFile).
pub(crate) async fn fallocate_windows<D: DiskAdaptor>(
    adaptor: &mut D,
    length: u64,
    secure: bool,
) -> Result<()> {
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
        // Validate length fits in i64 for SetFileValidData
        if length > i64::MAX as u64 {
            return Err(Aria2Error::Io(
                "SetFileValidData length exceeds i64 range".into(),
            ));
        }
        // Extend the valid data length up to `length`, forcing the
        // filesystem to allocate real blocks rather than a sparse hole.
        // SAFETY: handle is a valid file handle obtained from
        // adaptor.windows_raw_handle(). length as i64 is safe for
        // practical file sizes (< 2^63 bytes). SetFileValidData is
        // a standard Win32 API call.
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::SetFileValidData(handle, length as i64)
        };
        if ok == 0 {
            // SAFETY: GetLastError() is always safe to call on Windows.
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
            strategies::async_zero_fill(adaptor, length).await
        } else {
            super::SECURE_FALLOC_WARN_ONCE.call_once(|| {
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
