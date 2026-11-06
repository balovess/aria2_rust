use super::*;

/// Add one or more HTTP(S)/FTP(S), SFTP, magnet, or torrent URIs.
///
/// # Safety
/// `session` must point to a live session. `uris` must be null only when
/// `uri_count` is zero; otherwise it must point to `uri_count` pointers to
/// valid NUL-terminated C strings. `options` must be null or point to
/// `option_count` valid key/value entries, and `gid_out` must point to a
/// writable `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_add_uri(
    session: *mut Aria2RustSession,
    uris: *const *const c_char,
    uri_count: usize,
    options: *const Aria2RustKeyValue,
    option_count: usize,
    gid_out: *mut u64,
) -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        if session.is_null() || (uri_count > 0 && uris.is_null()) || gid_out.is_null() {
            return INVALID_ARGUMENT;
        }
        // SAFETY: Pointer and length checks above establish the array bounds;
        // each URI is validated as a C string.
        let uri_ptrs = unsafe { slice::from_raw_parts(uris, uri_count) };
        let uris = match uri_ptrs
            .iter()
            .map(|uri| unsafe { read_c_string(*uri) })
            .collect::<std::result::Result<Vec<_>, _>>()
        {
            Ok(uris) => uris,
            Err(_) => return INVALID_ARGUMENT,
        };
        let options = match unsafe { read_key_values(options, option_count) } {
            Ok(options) => options,
            Err(_) => return INVALID_ARGUMENT,
        };
        // SAFETY: The null check above establishes exclusive access through
        // the opaque handle for this synchronous C call.
        let session = unsafe { &mut *session };
        match session.add_uri(uris, options) {
            Ok(gid) => {
                unsafe { *gid_out = gid };
                0
            }
            Err(error) => session.fail(error, INVALID_ARGUMENT),
        }
    })
}

/// Add a local torrent from its bencoded bytes, optionally with web-seed URIs.
/// The torrent is parsed before the task is queued, so file metadata is
/// available immediately when the task is paused.
///
/// # Safety
/// `session` must point to a live session. `torrent_data` must point to
/// `torrent_length` readable bytes. `web_seed_uris` must be null only when
/// `web_seed_uri_count` is zero; otherwise it must point to valid C strings.
/// `options` must be null or point to `option_count` valid key/value entries,
/// and `gid_out` must point to writable storage for one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_add_torrent(
    session: *mut Aria2RustSession,
    torrent_data: *const u8,
    torrent_length: usize,
    web_seed_uris: *const *const c_char,
    web_seed_uri_count: usize,
    options: *const Aria2RustKeyValue,
    option_count: usize,
    gid_out: *mut u64,
) -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        if session.is_null()
            || torrent_data.is_null()
            || torrent_length == 0
            || (web_seed_uri_count > 0 && web_seed_uris.is_null())
            || gid_out.is_null()
        {
            return INVALID_ARGUMENT;
        }
        // SAFETY: The caller contract supplies exactly `torrent_length`
        // readable bytes for this synchronous call.
        let data = unsafe { slice::from_raw_parts(torrent_data, torrent_length) }.to_vec();
        let web_seed_uris = if web_seed_uri_count == 0 {
            Vec::new()
        } else {
            // SAFETY: The pointer and count were checked above; each entry is
            // validated as a NUL-terminated C string.
            let uri_ptrs = unsafe { slice::from_raw_parts(web_seed_uris, web_seed_uri_count) };
            match uri_ptrs
                .iter()
                .map(|uri| unsafe { read_c_string(*uri) })
                .collect::<std::result::Result<Vec<_>, _>>()
            {
                Ok(uris) => uris,
                Err(_) => return INVALID_ARGUMENT,
            }
        };
        let options = match unsafe { read_key_values(options, option_count) } {
            Ok(options) => options,
            Err(_) => return INVALID_ARGUMENT,
        };
        let session = unsafe { &mut *session };
        match session.add_torrent(data, web_seed_uris, options) {
            Ok(gid) => {
                unsafe { *gid_out = gid };
                0
            }
            Err(error) => session.fail(error, INVALID_ARGUMENT),
        }
    })
}

/// Add Metalink data and copy the created numeric GIDs into `gids_out`.
/// `gid_count_out` always receives the total number of created GIDs. The
/// caller must provide enough capacity for the complete result; otherwise no
/// groups are queued and `ARIA2_RUST_BUFFER_TOO_SMALL` is returned.
///
/// # Safety
/// `session` must be live. `metalink_data` must reference `data_length` bytes;
/// options and their strings must be valid for this call. `gid_count_out` must
/// be writable. `gids_out` may be null only when `gid_capacity` is zero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_add_metalink(
    session: *mut Aria2RustSession,
    metalink_data: *const u8,
    data_length: usize,
    options: *const Aria2RustKeyValue,
    option_count: usize,
    gids_out: *mut u64,
    gid_capacity: usize,
    gid_count_out: *mut usize,
) -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        if session.is_null()
            || metalink_data.is_null()
            || data_length == 0
            || gid_count_out.is_null()
            || (gid_capacity > 0 && gids_out.is_null())
        {
            return INVALID_ARGUMENT;
        }
        let data = unsafe { slice::from_raw_parts(metalink_data, data_length) }.to_vec();
        let options = match unsafe { read_key_values(options, option_count) } {
            Ok(options) => options,
            Err(_) => return INVALID_ARGUMENT,
        };
        let session = unsafe { &mut *session };
        let gids = match session.add_metalink(data, options, Some(gid_capacity)) {
            Ok(gids) => gids,
            Err(error) if error.starts_with("output buffer too small;") => {
                return session.fail(error, BUFFER_TOO_SMALL);
            }
            Err(error) => return session.fail(error, INVALID_ARGUMENT),
        };
        unsafe { *gid_count_out = gids.len() };
        if !gids_out.is_null() {
            let destination = unsafe { slice::from_raw_parts_mut(gids_out, gid_capacity) };
            for (slot, gid) in destination.iter_mut().zip(gids.iter().copied()) {
                *slot = gid;
            }
        }
        0
    })
}
