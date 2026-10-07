use super::*;

/// Get a status/progress snapshot for a GID. Returns 0 on success, -1 absent.
///
/// # Safety
/// `session` must point to a live session, `output` must point to writable
/// storage for one [`Aria2RustDownloadInfo`], and the session must not be
/// accessed through another mutable pointer concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_get_download_info(
    session: *mut Aria2RustSession,
    gid: u64,
    output: *mut Aria2RustDownloadInfo,
) -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        if session.is_null() || output.is_null() {
            return INVALID_ARGUMENT;
        }
        let session = unsafe { &mut *session };
        match session.get_info(gid) {
            Some(info) => {
                unsafe { *output = info };
                0
            }
            None => session.fail(format!("GID {gid} not found"), INVALID_ARGUMENT),
        }
    })
}

/// Return the number of file entries for a live or stopped download.
/// Returns zero when the GID is absent.
///
/// # Safety
/// `session` must be null or point to a live session not accessed concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_get_file_count(
    session: *mut Aria2RustSession,
    gid: u64,
) -> usize {
    ffi_result(0, || {
        if session.is_null() {
            return 0;
        }
        unsafe { (&*session).file_entries(gid).map_or(0, |files| files.len()) }
    })
}

/// Get size/progress metadata for one 1-based file index.
/// Returns 0 on success and -1 when the GID or file index is absent.
///
/// # Safety
/// `session` must point to a live session, `output` must point to writable
/// storage for one [`Aria2RustFileInfo`], and the session must not be accessed
/// through another mutable pointer concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_get_file_info(
    session: *mut Aria2RustSession,
    gid: u64,
    file_index: usize,
    output: *mut Aria2RustFileInfo,
) -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        if session.is_null() || output.is_null() || file_index == 0 {
            return INVALID_ARGUMENT;
        }
        let entry = unsafe { (&*session).file_entries(gid) }
            .and_then(|files| files.into_iter().nth(file_index - 1));
        match entry {
            Some(entry) => {
                unsafe {
                    *output = Aria2RustFileInfo {
                        length: entry.length,
                        completed_length: entry.completed_length,
                        selected: u8::from(entry.selected),
                    };
                }
                0
            }
            None => unsafe { (&mut *session).fail("file entry not found", INVALID_ARGUMENT) },
        }
    })
}

/// Copy the path for one 1-based file index into a caller-owned buffer.
/// Returns required bytes including the NUL terminator, or zero when absent.
/// A short buffer is not written; call once with NULL/0 to query the size.
///
/// # Safety
/// `session` must point to a live session. If `capacity` is non-zero,
/// `output` must point to a writable buffer of at least `capacity` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_get_file_path(
    session: *mut Aria2RustSession,
    gid: u64,
    file_index: usize,
    output: *mut c_char,
    capacity: usize,
) -> usize {
    ffi_result(0, || {
        if session.is_null() || file_index == 0 {
            return 0;
        }
        let Some(path) = (unsafe { (&*session).file_entries(gid) })
            .and_then(|files| files.into_iter().nth(file_index - 1))
            .map(|entry| entry.path)
        else {
            return 0;
        };
        write_c_string(&path, output, capacity)
    })
}

/// Get aggregate session statistics.
///
/// # Safety
/// `session` must point to a live session, `output` must point to writable
/// storage for one [`Aria2RustGlobalStat`], and the session must not be
/// accessed through another mutable pointer concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_get_global_stat(
    session: *mut Aria2RustSession,
    output: *mut Aria2RustGlobalStat,
) -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        if session.is_null() || output.is_null() {
            return INVALID_ARGUMENT;
        }
        let stat = unsafe { (&mut *session).global_stat() };
        unsafe { *output = stat };
        0
    })
}

/// Copy active GIDs into a caller-owned array. Returns the required count.
///
/// # Safety
/// `session` must point to a live session. If `capacity` is non-zero,
/// `output` must point to an array of at least `capacity` writable `u64`
/// values. The session must not be accessed through another mutable pointer
/// concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_get_active_downloads(
    session: *mut Aria2RustSession,
    output: *mut u64,
    capacity: usize,
) -> usize {
    ffi_result(0, || {
        if session.is_null() || (capacity > 0 && output.is_null()) {
            return 0;
        }
        let gids = unsafe { (&*session).request_man.get_active_groups() };
        let values = gids
            .iter()
            .map(|group| group.recover().gid().value())
            .collect::<Vec<_>>();
        if !output.is_null() {
            // SAFETY: `capacity` is the caller-provided output capacity.
            let destination = unsafe { slice::from_raw_parts_mut(output, capacity) };
            for (slot, gid) in destination.iter_mut().zip(values.iter().copied()) {
                *slot = gid;
            }
        }
        values.len()
    })
}
