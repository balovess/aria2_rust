use super::*;

/// Initialize process-global engine dependencies.
#[unsafe(no_mangle)]
pub extern "C" fn aria2_rust_library_init() -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        LIBRARY_INITIALIZED.store(true, Ordering::Release);
        0
    })
}

/// Release process-global engine dependencies.
#[unsafe(no_mangle)]
pub extern "C" fn aria2_rust_library_deinit() -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        LIBRARY_INITIALIZED.store(false, Ordering::Release);
        0
    })
}

/// Create a session. Unknown options are ignored, matching aria2's C++ API.
///
/// # Safety
/// `options` must be null or point to `option_count` valid
/// [`Aria2RustKeyValue`] entries. Each non-null name and value pointer in
/// those entries must reference a valid NUL-terminated C string. The returned
/// opaque pointer must be released with [`aria2_rust_session_final`] exactly
/// once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_session_new(
    options: *const Aria2RustKeyValue,
    option_count: usize,
    _user_data: *mut c_void,
) -> *mut Aria2RustSession {
    ffi_result(ptr::null_mut(), || {
        if !LIBRARY_INITIALIZED.load(Ordering::Acquire) {
            return ptr::null_mut();
        }
        let options = match unsafe { read_key_values(options, option_count) } {
            Ok(options) => options,
            Err(_) => return ptr::null_mut(),
        };
        match Aria2RustSession::new(options) {
            Ok(session) => Box::into_raw(Box::new(session)),
            Err(_) => ptr::null_mut(),
        }
    })
}

/// Finalize and destroy a session. Passing NULL is safe.
///
/// # Safety
/// `session` must be null or a pointer previously returned by
/// [`aria2_rust_session_new`] that has not already been finalized or freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_session_final(session: *mut Aria2RustSession) -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        if session.is_null() {
            return 0;
        }
        // SAFETY: Ownership is transferred exactly once by this function.
        let mut session = unsafe { Box::from_raw(session) };
        session.finalize()
    })
}
