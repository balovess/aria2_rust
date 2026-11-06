use super::*;

/// Convert a numeric GID to the canonical 16-character lowercase form.
#[unsafe(no_mangle)]
pub extern "C" fn aria2_rust_gid_to_hex(gid: u64, output: *mut c_char, capacity: usize) -> usize {
    ffi_result(0, || {
        write_c_string(&GroupId::new(gid).to_hex_string(), output, capacity)
    })
}

/// Parse a hexadecimal GID. Invalid input returns zero.
///
/// # Safety
/// `input` must be null or point to a valid NUL-terminated C string readable
/// for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_hex_to_gid(input: *const c_char) -> u64 {
    ffi_result(0, || {
        let Ok(input) = (unsafe { read_c_string(input) }) else {
            return 0;
        };
        GroupId::from_hex_string(&input).map_or(0, |gid| gid.value())
    })
}

/// Return whether a GID is the null sentinel.
#[unsafe(no_mangle)]
pub extern "C" fn aria2_rust_is_null_gid(gid: u64) -> u8 {
    u8::from(gid == 0)
}

/// Copy the latest session error into a caller-owned buffer. Returns bytes
/// required including the NUL terminator.
///
/// # Safety
/// `session` must be null or point to a live session. If `capacity` is
/// non-zero, `output` must point to a writable buffer of at least `capacity`
/// bytes. The session must not be accessed through another mutable pointer
/// concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_last_error(
    session: *mut Aria2RustSession,
    output: *mut c_char,
    capacity: usize,
) -> usize {
    ffi_result(0, || {
        if session.is_null() {
            return 0;
        }
        // SAFETY: The caller owns the opaque session during this synchronous
        // call.
        let session = unsafe { &mut *session };
        write_c_string(&session.last_error, output, capacity)
    })
}
