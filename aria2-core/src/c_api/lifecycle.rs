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
/// once. This constructor ignores `user_data`; use the callback-enabled
/// constructor to receive it with lifecycle events.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_session_new(
    options: *const Aria2RustKeyValue,
    option_count: usize,
    _user_data: *mut c_void,
) -> *mut Aria2RustSession {
    unsafe { create_session(options, option_count, None, ptr::null_mut()) }
}

/// Create a session and register a lifecycle callback before returning it.
///
/// Event values use the original aria2 numbering: Start=1, Pause=2, Stop=3,
/// Complete=4, Error=5, and BitTorrent payload-complete=6. The callback is
/// invoked only for downloads owned by this session. It runs synchronously on
/// the engine event thread, so it must be fast, must not unwind, and must not
/// call the same session's C API. `user_data` is opaque and borrowed until
/// `aria2_rust_session_final` returns. The callback's integer result is ignored.
/// A NULL callback disables event delivery.
///
/// # Safety
/// The option pointers follow [`aria2_rust_session_new`]'s requirements. If
/// `callback` is non-NULL, `user_data` must remain valid for the lifetime of
/// the session and the callback must obey the threading and reentrancy rules
/// above. The returned pointer must be finalized exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_session_new_with_download_event_callback(
    options: *const Aria2RustKeyValue,
    option_count: usize,
    callback: Option<Aria2RustDownloadEventCallback>,
    user_data: *mut c_void,
) -> *mut Aria2RustSession {
    unsafe { create_session(options, option_count, callback, user_data) }
}

unsafe fn create_session(
    options: *const Aria2RustKeyValue,
    option_count: usize,
    callback: Option<Aria2RustDownloadEventCallback>,
    user_data: *mut c_void,
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
            Ok(session) => {
                let mut session = Box::new(session);
                if let Some(callback) = callback {
                    let session_ptr = &mut *session as *mut Aria2RustSession;
                    let registration = events::DownloadEventCallbackRegistration::new(
                        session_ptr,
                        session.request_man.event_scope_id(),
                        user_data,
                        callback,
                    );
                    session.download_event_callback = Some(registration);
                }
                Box::into_raw(session)
            }
            Err(_) => ptr::null_mut(),
        }
    })
}

/// Finalize and destroy a session. Passing NULL is safe.
///
/// # Safety
/// `session` must be null or a pointer previously returned by
/// [`aria2_rust_session_new`] or
/// [`aria2_rust_session_new_with_download_event_callback`] that has not already
/// been finalized or freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_session_final(session: *mut Aria2RustSession) -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        if session.is_null() {
            return 0;
        }
        // SAFETY: Ownership is transferred exactly once by this function.
        let mut session = unsafe { Box::from_raw(session) };
        let result = session.finalize();
        drop(session.download_event_callback.take());
        result
    })
}
