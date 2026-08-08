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
use tokio::sync::{RwLock, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::config::{ConfigManager, OptionValue};
use crate::engine::download_engine::DownloadEngine;
use crate::engine::engine_command::EngineCommand;
use crate::error::Result;
use crate::rate_limiter::RateLimiterConfig;
use crate::request::request_group::{
    DownloadOptions, DownloadStatus, GroupId, HaltReason, RequestGroup,
};
use crate::request::request_group_man::RequestGroupMan;
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

/// Opaque session owned by the embedding application.
pub struct Aria2RustSession {
    runtime: Runtime,
    config: ConfigManager,
    request_man: Arc<RwLock<RequestGroupMan>>,
    command_tx: mpsc::UnboundedSender<EngineCommand>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    engine_task: Option<JoinHandle<Result<()>>>,
    keep_running: bool,
    last_error: String,
}

static LIBRARY_INITIALIZED: AtomicBool = AtomicBool::new(false);

const INVALID_ARGUMENT: i32 = -1;
const INTERNAL_ERROR: i32 = -2;

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

impl Aria2RustSession {
    fn new(raw_options: Vec<(String, String)>) -> std::result::Result<Self, String> {
        let runtime = Runtime::new().map_err(|error| format!("runtime init failed: {error}"))?;
        let (config, keep_running) = runtime.block_on(async {
            let mut config = ConfigManager::new();
            let mut keep_running = false;
            for (name, value) in raw_options {
                if name == "keep-running" {
                    keep_running = parse_bool(&value)?;
                    continue;
                }
                // aria2's C++ API ignores unknown options passed to
                // sessionNew. Known options still go through the registry so
                // type/range errors are reported instead of silently changing
                // the effective configuration.
                if !config.registry().contains(&name) {
                    continue;
                }
                config
                    .set_global_option(&name, OptionValue::Str(value))
                    .await
                    .map_err(|error| format!("invalid option {name}: {error}"))?;
            }
            Ok::<_, String>((config, keep_running))
        })?;

        let request_man = Arc::new(RwLock::new(RequestGroupMan::new()));
        let mut engine = DownloadEngine::new(100);
        engine.set_request_group_man(Arc::clone(&request_man));
        // The C API keeps the event loop alive between synchronous `run` calls,
        // matching the original library's RUN_ONCE mode.
        engine.set_keep_alive(true);

        let max_concurrent = runtime.block_on(config.get_global_i64("max-concurrent-downloads"));
        if let Some(max) = max_concurrent.filter(|value| *value >= 0) {
            runtime.block_on(async {
                request_man.read().await.set_max_concurrent(max as u32);
            });
        }

        let download_limit = runtime
            .block_on(config.get_global_i64("max-overall-download-limit"))
            .and_then(non_zero_limit);
        let upload_limit = runtime
            .block_on(config.get_global_i64("max-overall-upload-limit"))
            .and_then(non_zero_limit);
        if download_limit.is_some() || upload_limit.is_some() {
            engine.set_global_rate_limiter(RateLimiterConfig::new(download_limit, upload_limit));
            runtime.block_on(async {
                request_man
                    .read()
                    .await
                    .set_global_speed_limit(download_limit, upload_limit);
            });
        }

        let command_tx = engine.engine_command_sender();
        let shutdown_tx = engine.take_shutdown_sender();
        let engine_task = runtime.spawn(engine.run());

        Ok(Self {
            runtime,
            config,
            request_man,
            command_tx,
            shutdown_tx,
            engine_task: Some(engine_task),
            keep_running,
            last_error: String::new(),
        })
    }

    fn fail<T>(&mut self, message: impl Into<String>, fallback: T) -> T {
        self.last_error = message.into();
        fallback
    }

    fn merged_options(
        &mut self,
        overrides: Vec<(String, String)>,
    ) -> std::result::Result<DownloadOptions, String> {
        let values = self.runtime.block_on(self.config.get_all_global_options());
        let mut strings = values
            .into_iter()
            .filter_map(|(name, value)| {
                if value.is_none() {
                    None
                } else {
                    Some((name, value.to_string()))
                }
            })
            .collect::<HashMap<_, _>>();

        for (name, value) in overrides {
            if name == "keep-running" {
                continue;
            }
            let Some(definition) = self.config.registry().get(&name) else {
                continue;
            };
            let parsed = definition
                .parse_value(&value)
                .map_err(|error| format!("invalid option {name}: {error}"))?;
            strings.insert(name, parsed.to_string());
        }
        Ok(DownloadOptions::from_option_strings(&strings))
    }

    fn add_uri(
        &mut self,
        uris: Vec<String>,
        overrides: Vec<(String, String)>,
    ) -> std::result::Result<u64, String> {
        if uris.is_empty() || uris.iter().any(String::is_empty) {
            return Err("at least one non-empty URI is required".to_string());
        }
        let options = self.merged_options(overrides)?;
        let gid = self.runtime.block_on(async {
            let man = self.request_man.read().await;
            let gid = man.next_available_gid();
            let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
                gid,
                uris,
                options.clone(),
            )));
            if options.uses_memory_download() {
                group.recover().mark_in_memory_download();
            }
            if options.pause {
                group
                    .recover_mut()
                    .pause()
                    .map_err(|error| error.to_string())?;
            }
            man.add_group_arc(group);
            Ok::<_, String>(gid.value())
        })?;
        Ok(gid)
    }

    fn run(&mut self, mode: u32) -> i32 {
        let keep_running = self.keep_running;
        self.runtime.block_on(async {
            if mode == 1 {
                if !keep_running && self.request_man.read().await.download_finished() {
                    return 0;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                return if keep_running || !self.request_man.read().await.download_finished() {
                    1
                } else {
                    0
                };
            }
            loop {
                if self.request_man.read().await.download_finished() {
                    return 0;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    }

    fn finalize(&mut self) -> i32 {
        let _ = self.command_tx.send(EngineCommand::ForceHaltAll {
            reason: HaltReason::ShutdownSignal,
        });
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        if let Some(engine_task) = self.engine_task.take() {
            let _ = self.runtime.block_on(engine_task);
        }
        0
    }

    fn change_global_options(&mut self, options: Vec<(String, String)>) -> i32 {
        let mut download_limit_changed = false;
        let mut upload_limit_changed = false;
        let mut max_concurrent = None;
        for (name, value) in options {
            if name == "keep-running" {
                match parse_bool(&value) {
                    Ok(value) => self.keep_running = value,
                    Err(error) => return self.fail(error, INVALID_ARGUMENT),
                }
                continue;
            }
            if !self.config.registry().contains(&name) {
                continue;
            }
            let parsed = match self
                .config
                .registry()
                .get(&name)
                .and_then(|def| def.parse_value(&value).ok())
            {
                Some(value) => value,
                None => return self.fail(format!("invalid option {name}"), INVALID_ARGUMENT),
            };
            if self
                .runtime
                .block_on(
                    self.config
                        .set_global_option(&name, OptionValue::Str(value.clone())),
                )
                .is_err()
            {
                return self.fail(format!("invalid option {name}"), INVALID_ARGUMENT);
            }
            match name.as_str() {
                "max-concurrent-downloads" => max_concurrent = parsed.as_i64(),
                "max-overall-download-limit" => download_limit_changed = true,
                "max-overall-upload-limit" => upload_limit_changed = true,
                _ => {}
            }
        }

        if let Some(max) = max_concurrent.filter(|value| *value >= 0) {
            let _ = self
                .command_tx
                .send(EngineCommand::SetMaxConcurrent { max: max as u32 });
        }
        if download_limit_changed || upload_limit_changed {
            let download_limit = self
                .runtime
                .block_on(self.config.get_global_i64("max-overall-download-limit"))
                .and_then(non_zero_limit);
            let upload_limit = self
                .runtime
                .block_on(self.config.get_global_i64("max-overall-upload-limit"))
                .and_then(non_zero_limit);
            let _ = self.command_tx.send(EngineCommand::SetGlobalRateLimit {
                download_limit,
                upload_limit,
            });
        }
        0
    }

    fn change_options(&mut self, gid: u64, options: Vec<(String, String)>) -> i32 {
        let Some(group) = self
            .runtime
            .block_on(self.request_man.read())
            .find_group(GroupId::new(gid))
        else {
            return self.fail(format!("GID {gid} not found"), INVALID_ARGUMENT);
        };
        let mut changes = HashMap::new();
        for (name, value) in options {
            let Some(definition) = self.config.registry().get(&name) else {
                continue;
            };
            let parsed = match definition.parse_value(&value) {
                Ok(value) => value,
                Err(error) => return self.fail(error, INVALID_ARGUMENT),
            };
            if crate::request::request_group::is_option_changeable(
                &name,
                matches!(group.recover().status(), DownloadStatus::Active),
            ) == crate::request::request_group::ChangeableKind::NotChangeable
            {
                continue;
            }
            changes.insert(name, (&parsed).into());
        }
        if changes.is_empty() {
            return 0;
        }
        let update_result = {
            let manager = self.runtime.block_on(self.request_man.read());
            manager.update_group_options(&GroupId::new(gid).to_hex_string(), changes)
        };
        if let Err(error) = update_result {
            return self.fail(error, INVALID_ARGUMENT);
        }
        0
    }

    fn get_info(&mut self, gid: u64) -> Option<Aria2RustDownloadInfo> {
        let manager = self.runtime.block_on(self.request_man.read());
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

    fn global_stat(&mut self) -> Aria2RustGlobalStat {
        let manager = self.runtime.block_on(self.request_man.read());
        let mut stat = Aria2RustGlobalStat {
            num_stopped: manager.stopped_count() as u64,
            ..Default::default()
        };
        for group in manager.list_groups() {
            let group = group.recover();
            match group.status() {
                DownloadStatus::Active => {
                    stat.num_active += 1;
                    stat.download_speed =
                        stat.download_speed.saturating_add(group.download_speed());
                    stat.upload_speed = stat.upload_speed.saturating_add(group.upload_speed());
                }
                DownloadStatus::Waiting | DownloadStatus::Paused => stat.num_waiting += 1,
                DownloadStatus::Complete | DownloadStatus::Error(_) | DownloadStatus::Removed => {}
            }
        }
        stat
    }
}

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
/// opaque pointer must be released with [`aria2_rust_session_final`] or
/// [`aria2_rust_session_free`] exactly once.
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

/// Alias for callers that prefer an explicit free operation.
///
/// # Safety
/// `session` must be null or a pointer previously returned by
/// [`aria2_rust_session_new`] that has not already been finalized or freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aria2_rust_session_free(session: *mut Aria2RustSession) {
    let _ = unsafe { aria2_rust_session_final(session) };
}

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

/// Poll the engine. Mode 0 waits for all downloads; mode 1 performs one poll.
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
            let manager = session.runtime.block_on(session.request_man.read());
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
            .runtime
            .block_on(session.request_man.read())
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
        let result = session
            .runtime
            .block_on(session.request_man.read())
            .unpause_group(GroupId::new(gid));
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
        let gids = unsafe {
            (&mut *session)
                .runtime
                .block_on(async { (&*session).request_man.read().await.get_active_groups() })
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn kv(name: &str, value: &str) -> (Aria2RustKeyValue, CString, CString) {
        let name_c = CString::new(name).unwrap();
        let value_c = CString::new(value).unwrap();
        (
            Aria2RustKeyValue {
                name: name_c.as_ptr(),
                value: value_c.as_ptr(),
            },
            name_c,
            value_c,
        )
    }

    #[test]
    fn c_api_session_lifecycle_and_queue_controls() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(aria2_rust_library_init(), 0);
        let (pause_entry, pause_name, pause_value) = kv("pause", "true");
        let session = unsafe { aria2_rust_session_new(&pause_entry, 1, ptr::null_mut()) };
        let _keep_alive = [pause_name, pause_value];
        assert!(!session.is_null());

        let uri = CString::new("http://127.0.0.1:1/not-started").unwrap();
        let uri_ptr = uri.as_ptr();
        let mut gid = 0;
        assert_eq!(
            unsafe { aria2_rust_add_uri(session, &uri_ptr, 1, ptr::null(), 0, &mut gid,) },
            0
        );
        assert_ne!(gid, 0);

        let mut info = Aria2RustDownloadInfo::default();
        let mut initial_info_result = -1;
        for _ in 0..100 {
            initial_info_result = unsafe { aria2_rust_get_download_info(session, gid, &mut info) };
            if initial_info_result == 0 {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(initial_info_result, 0);
        assert_eq!(info.status, Aria2RustDownloadStatus::Paused as u32);

        let mut unpause_result = -1;
        for _ in 0..100 {
            unpause_result = unsafe { aria2_rust_unpause(session, gid) };
            if unpause_result == 0 {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(unpause_result, 0);
        // Re-pause immediately so the short-lived test URI cannot race the
        // engine into a terminal network error before the state assertion.
        let mut pause_result = -1;
        for _ in 0..100 {
            pause_result = unsafe { aria2_rust_pause(session, gid, 1) };
            if pause_result == 0 {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(pause_result, 0);
        let mut info_result = -1;
        for _ in 0..100 {
            info_result = unsafe { aria2_rust_get_download_info(session, gid, &mut info) };
            if info_result == 0 {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(info_result, 0);
        assert_eq!(info.status, Aria2RustDownloadStatus::Paused as u32);

        let mut remove_result = -1;
        for _ in 0..100 {
            remove_result = unsafe { aria2_rust_remove(session, gid, 1) };
            if remove_result == 0 {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(remove_result, 0);
        let mut removed_info_result = -1;
        for _ in 0..2000 {
            removed_info_result = unsafe { aria2_rust_get_download_info(session, gid, &mut info) };
            if removed_info_result == 0 && info.status == Aria2RustDownloadStatus::Removed as u32 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(removed_info_result, 0);
        assert_eq!(info.status, Aria2RustDownloadStatus::Removed as u32);

        assert_eq!(unsafe { aria2_rust_session_final(session) }, 0);
        assert_eq!(aria2_rust_library_deinit(), 0);
    }

    #[test]
    fn c_api_uses_shared_option_conversion_and_ignores_unknown_options() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(aria2_rust_library_init(), 0);
        let (entry, name, value) = kv("split", "4");
        let (unknown, unknown_name, unknown_value) = kv("not-an-aria2-option", "ignored");
        let entries = [entry, unknown];
        let _keep_alive = [name, value, unknown_name, unknown_value];
        let session =
            unsafe { aria2_rust_session_new(entries.as_ptr(), entries.len(), ptr::null_mut()) };
        assert!(!session.is_null());
        let uri = CString::new("http://127.0.0.1:1/option-test").unwrap();
        let uri_ptr = uri.as_ptr();
        let mut gid = 0;
        assert_eq!(
            unsafe { aria2_rust_add_uri(session, &uri_ptr, 1, ptr::null(), 0, &mut gid,) },
            0
        );
        assert_ne!(gid, 0);
        let mut hex = [0 as c_char; 17];
        assert_eq!(aria2_rust_gid_to_hex(gid, hex.as_mut_ptr(), hex.len()), 17);
        assert_eq!(unsafe { aria2_rust_hex_to_gid(hex.as_ptr()) }, gid);
        unsafe { aria2_rust_session_final(session) };
        aria2_rust_library_deinit();
    }
}
