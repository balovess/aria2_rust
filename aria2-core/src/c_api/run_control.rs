use super::*;

/// Advance the engine. Mode 0 waits for all downloads; mode 1 yields once to
/// let queued engine commands run and then returns the current keep-running
/// state without an arbitrary wall-clock delay.
///
/// # Safety
/// `session` must point to a live session and must not be accessed through
/// another mutable pointer concurrently with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_run(session: *mut Aria2RustSession, mode: u32) -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        if session.is_null() || mode > 1 {
            return INVALID_ARGUMENT;
        }
        // SAFETY: See `aria2_rust_add_uri`.
        unsafe { (&mut *session).run(mode) }
    })
}

/// Wait for one download to reach a terminal state without polling.
///
/// `timeout_ms == 0` waits indefinitely. On success, `output` receives the
/// final status snapshot. `ARIA2_RUST_TIMEOUT` means that only this wait
/// expired; the download remains unchanged.
///
/// # Safety
/// `session` and `output` must point to live writable objects, and the
/// session must not be accessed through another mutable pointer concurrently
/// with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_wait_download(
    session: *mut Aria2RustSession,
    gid: u64,
    timeout_ms: u64,
    output: *mut Aria2RustDownloadInfo,
) -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        if session.is_null() || output.is_null() {
            return INVALID_ARGUMENT;
        }
        let session = unsafe { &mut *session };
        match session.wait_download(gid, timeout_ms) {
            Ok(info) => {
                unsafe { *output = info };
                0
            }
            Err(TIMEOUT) => session.fail("download wait timed out", TIMEOUT),
            Err(INVALID_ARGUMENT) => session.fail(format!("GID {gid} not found"), INVALID_ARGUMENT),
            Err(error) => session.fail(format!("download wait failed: {error}"), INTERNAL_ERROR),
        }
    })
}

/// Remove a download by numeric GID. `force` is non-zero for force removal.
///
/// # Safety
/// `session` must point to a live session and must not be accessed through
/// another mutable pointer concurrently with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_remove(
    session: *mut Aria2RustSession,
    gid: u64,
    force: u8,
) -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        if session.is_null() {
            return INVALID_ARGUMENT;
        }
        let session = unsafe { &mut *session };
        let removal_result = {
            let manager = &session.request_man;
            if force != 0 {
                manager.force_remove_group(GroupId::new(gid))
            } else {
                manager.remove_group(GroupId::new(gid))
            }
        };
        if let Err(error) = removal_result {
            return session.fail(error.to_string(), INVALID_ARGUMENT);
        }
        let command = if force != 0 {
            EngineCommand::ForceRemoveDownload {
                gid: GroupId::new(gid),
            }
        } else {
            EngineCommand::RemoveDownload {
                gid: GroupId::new(gid),
            }
        };
        session
            .command_tx
            .send(command)
            .map(|_| 0)
            .unwrap_or_else(|error| session.fail(error.to_string(), INTERNAL_ERROR))
    })
}

/// Pause a download by numeric GID.
///
/// # Safety
/// `session` must point to a live session and must not be accessed through
/// another mutable pointer concurrently with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_pause(
    session: *mut Aria2RustSession,
    gid: u64,
    force: u8,
) -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        if session.is_null() {
            return INVALID_ARGUMENT;
        }
        let session = unsafe { &mut *session };
        let result = session
            .request_man
            .find_group(GroupId::new(gid))
            .ok_or_else(|| format!("GID {gid} not found"));
        let Some(group) = result.ok() else {
            return session.fail(format!("GID {gid} not found"), INVALID_ARGUMENT);
        };
        let command = if force != 0 {
            EngineCommand::ForcePause {
                gid: GroupId::new(gid),
            }
        } else {
            EngineCommand::Pause {
                gid: GroupId::new(gid),
            }
        };
        // Apply the state transition synchronously, then let the engine wake
        // the running command and account for its completion.
        let pause_result = {
            let mut group = group.recover_mut();
            if force != 0 {
                group.force_pause()
            } else {
                group.pause()
            }
        };
        if let Err(error) = pause_result {
            return session.fail(error.to_string(), INVALID_ARGUMENT);
        }
        session
            .command_tx
            .send(command)
            .map(|_| 0)
            .unwrap_or_else(|error| session.fail(error.to_string(), INTERNAL_ERROR))
    })
}

/// Unpause a paused download and make it eligible for promotion.
///
/// # Safety
/// `session` must point to a live session and must not be accessed through
/// another mutable pointer concurrently with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_unpause(session: *mut Aria2RustSession, gid: u64) -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        if session.is_null() {
            return INVALID_ARGUMENT;
        }
        let session = unsafe { &mut *session };
        let result = session.request_man.unpause_group(GroupId::new(gid));
        if let Err(error) = result {
            return session.fail(error.to_string(), INVALID_ARGUMENT);
        }
        session
            .command_tx
            .send(EngineCommand::Unpause {
                gid: GroupId::new(gid),
            })
            .map(|_| 0)
            .unwrap_or_else(|error| session.fail(error.to_string(), INTERNAL_ERROR))
    })
}

/// Pause all non-terminal downloads. `force` selects the force-pause variant.
///
/// # Safety
/// `session` must point to a live session and must not be accessed through
/// another mutable pointer concurrently with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_pause_all(session: *mut Aria2RustSession, force: u8) -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        if session.is_null() {
            return INVALID_ARGUMENT;
        }
        unsafe { (&mut *session).pause_all(force != 0) }
    })
}

/// Unpause all paused downloads.
///
/// # Safety
/// `session` must point to a live session and must not be accessed through
/// another mutable pointer concurrently with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_unpause_all(session: *mut Aria2RustSession) -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        if session.is_null() {
            return INVALID_ARGUMENT;
        }
        unsafe { (&mut *session).unpause_all() }
    })
}

/// Move a reserved download and write its resulting zero-based position.
/// Position modes are `ARIA2_RUST_POSITION_SET`, `ARIA2_RUST_POSITION_CUR`,
/// and `ARIA2_RUST_POSITION_END`.
///
/// # Safety
/// `session` and `position_out` must be valid writable pointers for the
/// duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_change_position(
    session: *mut Aria2RustSession,
    gid: u64,
    position: i32,
    mode: u32,
    position_out: *mut usize,
) -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        if session.is_null() || position_out.is_null() {
            return INVALID_ARGUMENT;
        }
        let session = unsafe { &mut *session };
        match session.change_position(gid, position, mode) {
            Ok(new_position) => {
                unsafe { *position_out = new_position };
                0
            }
            Err(error) => session.fail(error, INVALID_ARGUMENT),
        }
    })
}

/// Apply runtime-changeable options to a download.
///
/// # Safety
/// `session` must point to a live session. `options` must be null or point to
/// `option_count` valid key/value entries whose strings remain readable for
/// the duration of this call. The session must not be accessed through
/// another mutable pointer concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_change_option(
    session: *mut Aria2RustSession,
    gid: u64,
    options: *const Aria2RustKeyValue,
    option_count: usize,
) -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        if session.is_null() {
            return INVALID_ARGUMENT;
        }
        let options = match unsafe { read_key_values(options, option_count) } {
            Ok(options) => options,
            Err(_) => return INVALID_ARGUMENT,
        };
        unsafe { (&mut *session).change_options(gid, options) }
    })
}

/// Apply dynamic global options such as concurrency and bandwidth limits.
///
/// # Safety
/// `session` must point to a live session. `options` must be null or point to
/// `option_count` valid key/value entries whose strings remain readable for
/// the duration of this call. The session must not be accessed through
/// another mutable pointer concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_change_global_option(
    session: *mut Aria2RustSession,
    options: *const Aria2RustKeyValue,
    option_count: usize,
) -> i32 {
    ffi_result(INTERNAL_ERROR, || {
        if session.is_null() {
            return INVALID_ARGUMENT;
        }
        let options = match unsafe { read_key_values(options, option_count) } {
            Ok(options) => options,
            Err(_) => return INVALID_ARGUMENT,
        };
        unsafe { (&mut *session).change_global_options(options) }
    })
}
