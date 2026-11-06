//! Stable C ABI for embedding the Rust download engine.
//!
//! The original `aria2.h` is a C++ interface whose ABI depends on
//! `std::string`, `std::vector`, and C++ virtual dispatch. This module exposes
//! the same session-oriented operations through an explicitly C-compatible
//! opaque handle and caller-owned buffers. It is a source-level migration
//! interface, not a binary-compatible replacement for the C++ classes.

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::runtime::Runtime;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::config::{ConfigManager, OptionValue};
use crate::engine::download_engine::DownloadEngine;
use crate::engine::engine_command::{EngineCommand, EngineCommandSender};
#[cfg(feature = "metalink")]
use crate::engine::metalink::to_request_group::MetalinkToRequestGroup;
use crate::error::Result;
use crate::rate_limiter::RateLimiterConfig;
use crate::request::request_group::{
    DownloadOptions, DownloadStatus, GroupId, HaltReason, RequestGroup,
};
use crate::request::request_group_man::{PositionMode, RequestGroupMan};
use crate::util::rwlock_ext::RwLockRecover;

/// C-compatible key/value option entry.
#[repr(C)]
pub struct Aria2RustKeyValue {
    pub name: *const c_char,
    pub value: *const c_char,
}

/// C-compatible status values matching `aria2::DownloadStatus` numbering.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aria2RustDownloadStatus {
    Active = 0,
    Waiting = 1,
    Paused = 2,
    Complete = 3,
    Error = 4,
    Removed = 5,
}

pub const ARIA2_RUST_POSITION_SET: u32 = 0;
pub const ARIA2_RUST_POSITION_CUR: u32 = 1;
pub const ARIA2_RUST_POSITION_END: u32 = 2;

pub const ARIA2_RUST_EVENT_DOWNLOAD_START: u32 = 1;
pub const ARIA2_RUST_EVENT_DOWNLOAD_PAUSE: u32 = 2;
pub const ARIA2_RUST_EVENT_DOWNLOAD_STOP: u32 = 3;
pub const ARIA2_RUST_EVENT_DOWNLOAD_COMPLETE: u32 = 4;
pub const ARIA2_RUST_EVENT_DOWNLOAD_ERROR: u32 = 5;
pub const ARIA2_RUST_EVENT_BT_DOWNLOAD_COMPLETE: u32 = 6;

/// Snapshot returned by `aria2_rust_get_download_info`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Aria2RustDownloadInfo {
    pub status: u32,
    pub total_length: u64,
    pub completed_length: u64,
    pub upload_length: u64,
    pub download_speed: u64,
    pub upload_speed: u64,
    pub error_code: u32,
}

/// File metadata returned by the C-compatible per-file query functions.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Aria2RustFileInfo {
    pub length: u64,
    pub completed_length: u64,
    pub selected: u8,
}

/// Snapshot returned by `aria2_rust_get_global_stat`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Aria2RustGlobalStat {
    pub download_speed: u64,
    pub upload_speed: u64,
    pub num_active: u64,
    pub num_waiting: u64,
    pub num_stopped: u64,
}

/// Callback invoked for lifecycle events belonging to one C API session.
///
/// The callback runs synchronously on the engine event thread. It must return
/// promptly, must not unwind across the C ABI, and must not re-enter the same
/// session's C API. The return value is ignored.
pub type Aria2RustDownloadEventCallback = unsafe extern "C" fn(
    session: *mut Aria2RustSession,
    event: u32,
    gid: u64,
    user_data: *mut c_void,
) -> i32;

/// Opaque session owned by the embedding application.
pub struct Aria2RustSession {
    runtime: Runtime,
    config: ConfigManager,
    request_man: Arc<RequestGroupMan>,
    command_tx: EngineCommandSender,
    shutdown_tx: Option<oneshot::Sender<()>>,
    engine_task: Option<JoinHandle<Result<()>>>,
    keep_running: bool,
    last_error: String,
    download_event_callback: Option<events::DownloadEventCallbackRegistration>,
}

static LIBRARY_INITIALIZED: AtomicBool = AtomicBool::new(false);

const INVALID_ARGUMENT: i32 = -1;
const INTERNAL_ERROR: i32 = -2;
const BUFFER_TOO_SMALL: i32 = -3;
const TIMEOUT: i32 = -4;

fn ffi_result<T>(fallback: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(fallback)
}

unsafe fn read_c_string(ptr: *const c_char) -> std::result::Result<String, String> {
    if ptr.is_null() {
        return Err("null C string".to_string());
    }
    // SAFETY: The caller owns the C ABI contract and must pass a NUL-terminated
    // string. `CStr` validates the byte sequence before conversion.
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map(str::to_string)
        .map_err(|_| "option contains invalid UTF-8".to_string())
}

unsafe fn read_key_values(
    options: *const Aria2RustKeyValue,
    count: usize,
) -> std::result::Result<Vec<(String, String)>, String> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if options.is_null() {
        return Err("options pointer is null".to_string());
    }
    // SAFETY: The caller supplies an array of `count` entries as required by
    // the public header. Each string is validated by `read_c_string`.
    let entries = unsafe { slice::from_raw_parts(options, count) };
    entries
        .iter()
        .map(|entry| {
            let name = unsafe { read_c_string(entry.name) }?;
            let value = unsafe { read_c_string(entry.value) }?;
            Ok((name, value))
        })
        .collect()
}

fn status_code(status: &DownloadStatus) -> u32 {
    match status {
        DownloadStatus::Active => Aria2RustDownloadStatus::Active as u32,
        DownloadStatus::Waiting => Aria2RustDownloadStatus::Waiting as u32,
        DownloadStatus::Paused => Aria2RustDownloadStatus::Paused as u32,
        DownloadStatus::Complete => Aria2RustDownloadStatus::Complete as u32,
        DownloadStatus::Error(_) => Aria2RustDownloadStatus::Error as u32,
        DownloadStatus::Removed => Aria2RustDownloadStatus::Removed as u32,
    }
}

fn result_info(result: &crate::request::request_group::DownloadResult) -> Aria2RustDownloadInfo {
    Aria2RustDownloadInfo {
        status: status_code(&result.status),
        total_length: result.total_length,
        completed_length: result.completed_length,
        upload_length: result.upload_length,
        download_speed: result.download_speed,
        upload_speed: result.upload_speed,
        error_code: result.code.as_code(),
    }
}

fn download_info_from_manager(
    manager: &RequestGroupMan,
    gid: u64,
) -> Option<Aria2RustDownloadInfo> {
    if let Some(group) = manager.find_group(GroupId::new(gid)) {
        let group = group.recover();
        return Some(Aria2RustDownloadInfo {
            status: status_code(&group.status()),
            total_length: group.total_length(),
            completed_length: group.completed_length(),
            upload_length: group.upload_length(),
            download_speed: group.download_speed(),
            upload_speed: group.upload_speed(),
            error_code: group.create_download_result().code.as_code(),
        });
    }
    manager
        .find_stopped_result(&GroupId::new(gid).to_hex_string())
        .as_ref()
        .map(result_info)
}

mod session;

fn parse_bool(value: &str) -> std::result::Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" | "on" => Ok(true),
        "false" | "no" | "0" | "off" => Ok(false),
        _ => Err(format!("invalid boolean '{value}'")),
    }
}

fn non_zero_limit(value: i64) -> Option<u64> {
    (value > 0).then_some(value as u64)
}

fn write_c_string(value: &str, output: *mut c_char, capacity: usize) -> usize {
    let required = value.len().saturating_add(1);
    if output.is_null() || capacity < required {
        return required;
    }
    // SAFETY: The caller supplied a writable buffer with `capacity` bytes and
    // the capacity check above reserves room for the terminator.
    unsafe {
        ptr::copy_nonoverlapping(value.as_ptr().cast::<c_char>(), output, value.len());
        *output.add(value.len()) = 0;
    }
    required
}

mod events;
mod gid;
mod lifecycle;
mod queries;
mod run_control;
mod task_add;

pub use gid::{
    aria2_rust_gid_to_hex, aria2_rust_hex_to_gid, aria2_rust_is_null_gid, aria2_rust_last_error,
};
pub use lifecycle::{
    aria2_rust_library_deinit, aria2_rust_library_init, aria2_rust_session_final,
    aria2_rust_session_new, aria2_rust_session_new_with_download_event_callback,
};
pub use queries::{
    aria2_rust_get_active_downloads, aria2_rust_get_download_info, aria2_rust_get_file_count,
    aria2_rust_get_file_info, aria2_rust_get_file_path, aria2_rust_get_global_stat,
};
pub use run_control::{
    aria2_rust_change_global_option, aria2_rust_change_option, aria2_rust_change_position,
    aria2_rust_pause, aria2_rust_pause_all, aria2_rust_remove, aria2_rust_run, aria2_rust_unpause,
    aria2_rust_unpause_all, aria2_rust_wait_download,
};
pub use task_add::{aria2_rust_add_metalink, aria2_rust_add_torrent, aria2_rust_add_uri};

#[cfg(test)]
#[path = "c_api/tests.rs"]
mod tests;
