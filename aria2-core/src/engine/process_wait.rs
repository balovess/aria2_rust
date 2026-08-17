//! OS-backed waiting for an arbitrary process to exit.
//!
//! The public shutdown watcher lives in `halt_watchers.rs`; this module keeps
//! platform-specific process handles out of that policy code. Native process
//! events are used wherever the target exposes them:
//!
//! - Windows: process handle plus a cancellation event
//! - Linux: `pidfd_open` registered with Tokio's `AsyncFd`
//! - macOS/BSD/Haiku: `kqueue` with `EVFILT_PROC | NOTE_EXIT`
//! - other targets: a low-frequency compatibility check, because no portable
//!   arbitrary-PID exit event is available there

use std::time::Duration;

#[cfg(unix)]
use std::io;
#[cfg(any(
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "haiku",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
))]
use std::os::fd::{AsRawFd, RawFd};

#[cfg(any(
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "haiku",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
))]
use tracing::debug;
use tracing::warn;

use super::engine_command::EngineCommandSender;

const FALLBACK_PROCESS_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessWaitResult {
    Exited,
    ChannelClosed,
}

/// Wait for a process-exit event, falling back only where the target OS has no
/// usable event primitive for an arbitrary PID.
#[cfg(windows)]
pub(crate) async fn wait_for_process_exit(
    pid: u32,
    cmd_tx: &EngineCommandSender,
) -> ProcessWaitResult {
    use windows_sys::Win32::System::Threading::CreateEventW;

    // The cancellation event lets the blocking wait be interrupted when the
    // engine shuts down. Aborting a `spawn_blocking` task would otherwise leave
    // a worker thread parked until the watched process exits.
    let cancel_handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) as usize };
    if cancel_handle == 0 {
        warn!(pid, "Failed to create process watcher cancellation event");
        return wait_for_process_exit_by_polling(pid, cmd_tx).await;
    }

    let mut cancel_signal = WindowsCancelSignal::new(cancel_handle);
    let mut wait_task =
        tokio::task::spawn_blocking(move || wait_for_process_exit_windows(pid, cancel_handle));

    let result = tokio::select! {
        wait_result = &mut wait_task => wait_result.unwrap_or(ProcessWaitResult::Exited),
        _ = cmd_tx.closed() => {
            cancel_signal.signal();
            wait_task.await.unwrap_or(ProcessWaitResult::ChannelClosed);
            ProcessWaitResult::ChannelClosed
        }
    };

    // The blocking waiter owns and closes the event after returning. Disarm
    // the guard so a completed wait is not signalled through a stale handle.
    cancel_signal.disarm();
    result
}

#[cfg(windows)]
struct WindowsCancelSignal {
    handle: usize,
}

#[cfg(windows)]
impl WindowsCancelSignal {
    fn new(handle: usize) -> Self {
        Self { handle }
    }

    fn signal(&self) {
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::System::Threading::SetEvent;

        // SAFETY: the blocking waiter owns the handle and only closes it after
        // observing the event or the process exit.
        unsafe {
            SetEvent(self.handle as HANDLE);
        }
    }

    fn disarm(&mut self) {
        self.handle = 0;
    }
}

#[cfg(windows)]
impl Drop for WindowsCancelSignal {
    fn drop(&mut self) {
        if self.handle != 0 {
            self.signal();
        }
    }
}

#[cfg(windows)]
fn wait_for_process_exit_windows(pid: u32, cancel_handle: usize) -> ProcessWaitResult {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForMultipleObjects,
    };

    // SAFETY: both handles are closed before returning. The event handle is
    // owned by this blocking waiter so an aborted async parent cannot leave it
    // dangling or close it while `WaitForMultipleObjects` is using it.
    unsafe {
        let process_handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if process_handle.is_null() {
            CloseHandle(cancel_handle as HANDLE);
            return ProcessWaitResult::Exited;
        }

        let handles = [process_handle, cancel_handle as HANDLE];
        let wait_result = WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE);
        CloseHandle(process_handle);
        CloseHandle(cancel_handle as HANDLE);

        if wait_result == WAIT_OBJECT_0 {
            ProcessWaitResult::Exited
        } else if wait_result == WAIT_OBJECT_0 + 1 {
            ProcessWaitResult::ChannelClosed
        } else {
            // A failed wait matches the old behaviour of treating an
            // inaccessible process as gone rather than hanging forever.
            ProcessWaitResult::Exited
        }
    }
}

#[cfg(target_os = "linux")]
struct PidFd(RawFd);

#[cfg(target_os = "linux")]
impl AsRawFd for PidFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

#[cfg(target_os = "linux")]
impl Drop for PidFd {
    fn drop(&mut self) {
        // SAFETY: this descriptor is owned by the wrapper and is closed once.
        unsafe {
            libc::close(self.0);
        }
    }
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: u32) -> io::Result<PidFd> {
    if pid == 0 || pid > libc::pid_t::MAX as u32 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "PID is outside the platform range",
        ));
    }

    // `pidfd_open` is exposed through libc's syscall constants so this still
    // builds against libc versions that predate the typed wrapper.
    // SAFETY: the syscall arguments are plain integer values.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(PidFd(fd as RawFd))
    }
}

#[cfg(target_os = "linux")]
pub(crate) async fn wait_for_process_exit(
    pid: u32,
    cmd_tx: &EngineCommandSender,
) -> ProcessWaitResult {
    let pidfd = match open_pidfd(pid) {
        Ok(pidfd) => pidfd,
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {
            return ProcessWaitResult::Exited;
        }
        Err(error) => {
            debug!(pid, error = %error, "pidfd_open unavailable; using compatibility watcher");
            return wait_for_process_exit_by_polling(pid, cmd_tx).await;
        }
    };

    let async_fd = match tokio::io::unix::AsyncFd::new(pidfd) {
        Ok(async_fd) => async_fd,
        Err(error) => {
            debug!(pid, error = %error, "Failed to register pidfd; using compatibility watcher");
            return wait_for_process_exit_by_polling(pid, cmd_tx).await;
        }
    };

    tokio::select! {
        result = async_fd.readable() => {
            if let Err(error) = result {
                warn!(pid, error = %error, "pidfd readiness failed; treating process as gone");
            }
            ProcessWaitResult::Exited
        }
        _ = cmd_tx.closed() => ProcessWaitResult::ChannelClosed,
    }
}

#[cfg(any(
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "haiku",
    target_os = "ios",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
))]
struct KqueueFd(RawFd);

#[cfg(any(
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "haiku",
    target_os = "ios",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
))]
impl AsRawFd for KqueueFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

#[cfg(any(
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "haiku",
    target_os = "ios",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
))]
impl Drop for KqueueFd {
    fn drop(&mut self) {
        // SAFETY: this descriptor is owned by the wrapper and is closed once.
        unsafe {
            libc::close(self.0);
        }
    }
}

#[cfg(any(
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "haiku",
    target_os = "ios",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
))]
fn open_process_kqueue(pid: u32) -> io::Result<KqueueFd> {
    if pid == 0 || pid > libc::pid_t::MAX as u32 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "PID is outside the platform range",
        ));
    }

    // SAFETY: `kqueue` takes no arguments and returns an owned descriptor.
    let kqueue_fd = unsafe { libc::kqueue() };
    if kqueue_fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let close_on_error = |error| {
        // SAFETY: this is the newly-created descriptor and no other owner exists.
        unsafe { libc::close(kqueue_fd) };
        Err(error)
    };

    // Tokio's `AsyncFd` requires a non-blocking descriptor.
    // SAFETY: `fcntl` operates on the descriptor created above.
    let flags = unsafe { libc::fcntl(kqueue_fd, libc::F_GETFL) };
    if flags < 0 {
        return close_on_error(io::Error::last_os_error());
    }
    // SAFETY: same descriptor, with the non-blocking flag added.
    if unsafe { libc::fcntl(kqueue_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return close_on_error(io::Error::last_os_error());
    }

    // SAFETY: zero-initialization is the portable way to handle the extra
    // fields present in newer FreeBSD `kevent` layouts.
    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
    event.ident = pid as libc::uintptr_t;
    event.filter = libc::EVFILT_PROC as _;
    event.flags = (libc::EV_ADD | libc::EV_ONESHOT) as _;
    event.fflags = libc::NOTE_EXIT as _;

    // SAFETY: all pointers reference valid storage for the duration of the call.
    let result = unsafe {
        libc::kevent(
            kqueue_fd,
            &event,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        )
    };
    if result < 0 {
        return close_on_error(io::Error::last_os_error());
    }

    Ok(KqueueFd(kqueue_fd))
}

#[cfg(any(
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "haiku",
    target_os = "ios",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
))]
pub(crate) async fn wait_for_process_exit(
    pid: u32,
    cmd_tx: &EngineCommandSender,
) -> ProcessWaitResult {
    let kqueue = match open_process_kqueue(pid) {
        Ok(kqueue) => kqueue,
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {
            return ProcessWaitResult::Exited;
        }
        Err(error) => {
            debug!(pid, error = %error, "kqueue process watch unavailable; using compatibility watcher");
            return wait_for_process_exit_by_polling(pid, cmd_tx).await;
        }
    };

    let async_fd = match tokio::io::unix::AsyncFd::new(kqueue) {
        Ok(async_fd) => async_fd,
        Err(error) => {
            debug!(pid, error = %error, "Failed to register kqueue; using compatibility watcher");
            return wait_for_process_exit_by_polling(pid, cmd_tx).await;
        }
    };

    tokio::select! {
        result = async_fd.readable() => {
            if let Err(error) = result {
                warn!(pid, error = %error, "kqueue readiness failed; treating process as gone");
            }
            ProcessWaitResult::Exited
        }
        _ = cmd_tx.closed() => ProcessWaitResult::ChannelClosed,
    }
}

#[cfg(not(any(
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "haiku",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
    windows,
)))]
pub(crate) async fn wait_for_process_exit(
    pid: u32,
    cmd_tx: &EngineCommandSender,
) -> ProcessWaitResult {
    wait_for_process_exit_by_polling(pid, cmd_tx).await
}

async fn wait_for_process_exit_by_polling(
    pid: u32,
    cmd_tx: &EngineCommandSender,
) -> ProcessWaitResult {
    loop {
        if !super::halt_watchers::is_process_alive(pid) {
            return ProcessWaitResult::Exited;
        }

        tokio::select! {
            _ = tokio::time::sleep(FALLBACK_PROCESS_POLL_INTERVAL) => {}
            _ = cmd_tx.closed() => return ProcessWaitResult::ChannelClosed,
        }
    }
}
