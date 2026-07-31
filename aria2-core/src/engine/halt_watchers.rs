//! Shutdown triggers driven by wall-clock time or an external process.
//!
//! Mirrors two C++ `TimeBasedCommand` subclasses:
//!
//! | C++ | option | this module |
//! |---|---|---|
//! | `TimedHaltCommand` | `--stop=N` | [`spawn_timed_halt`] |
//! | `WatchProcessCommand` | `--stop-with-process=PID` | [`spawn_process_watch`] |
//!
//! Both are modelled as detached tokio tasks rather than event-loop commands.
//! The C++ engine has to poll these on every tick because it owns a
//! single-threaded reactor; with tokio a sleeping task costs nothing and does
//! not couple shutdown policy to the loop's tick rate. The observable
//! behaviour is identical: when the trigger fires, an `EngineCommand::HaltAll`
//! (or `ForceHaltAll` when `force` is set) is pushed onto the same channel the
//! RPC layer and Ctrl+C handler use.
//!
//! Each task also terminates itself when the command channel closes, so a
//! finished engine never leaves a watcher behind.

use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::engine_command::EngineCommand;
use crate::request::request_group::HaltReason;

/// Poll interval for [`spawn_process_watch`].
///
/// C++ constructs `WatchProcessCommand` with `TimeBasedCommand(cuid, e, 1_s,
/// true)` — a one-second period. Kept identical so that the shutdown latency
/// after the watched process exits matches the original.
const PROCESS_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Send the appropriate halt command, returning `false` if the engine channel
/// is already closed (which means the engine is gone and the watcher should
/// stop).
fn send_halt(tx: &UnboundedSender<EngineCommand>, force: bool, reason: HaltReason) -> bool {
    let cmd = if force {
        EngineCommand::ForceHaltAll { reason }
    } else {
        EngineCommand::HaltAll { reason }
    };
    tx.send(cmd).is_ok()
}

/// Halt the engine after `duration` has elapsed.
///
/// Mirrors C++ `TimedHaltCommand` (`--stop=N`, `--stop` of `0` disables the
/// timer entirely — callers should not spawn a watcher in that case).
///
/// The returned [`JoinHandle`] can be dropped; the task is fully detached and
/// self-terminating.
pub fn spawn_timed_halt(
    cmd_tx: UnboundedSender<EngineCommand>,
    duration: Duration,
    force: bool,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        debug!(?duration, force, "Timed halt watcher armed");

        tokio::select! {
            _ = tokio::time::sleep(duration) => {}
            // The engine dropped its receiver — nothing left to halt.
            _ = cmd_tx.closed() => {
                debug!("Timed halt watcher exiting: engine channel closed");
                return;
            }
        }

        info!(
            seconds = duration.as_secs(),
            force, "Time has passed, commencing shutdown"
        );
        if !send_halt(&cmd_tx, force, HaltReason::ShutdownSignal) {
            warn!("Timed halt fired but the engine channel was already closed");
        }
    })
}

/// Halt the engine once the process identified by `pid` is no longer running.
///
/// Mirrors C++ `WatchProcessCommand` (`--stop-with-process=PID`). The process
/// is polled once per second; the very first poll happens immediately so that
/// an already-dead PID shuts the engine down without a spurious one-second
/// delay (C++ `TimeBasedCommand` is constructed with `routine = true`, which
/// also runs `process()` on the first tick).
pub fn spawn_process_watch(
    cmd_tx: UnboundedSender<EngineCommand>,
    pid: u32,
    force: bool,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        debug!(pid, force, "Process watcher armed");

        loop {
            if !is_process_alive(pid) {
                info!(pid, force, "Watched process is gone, commencing shutdown");
                if !send_halt(&cmd_tx, force, HaltReason::ShutdownSignal) {
                    warn!("Process watch fired but the engine channel was already closed");
                }
                return;
            }

            tokio::select! {
                _ = tokio::time::sleep(PROCESS_POLL_INTERVAL) => {}
                _ = cmd_tx.closed() => {
                    debug!(pid, "Process watcher exiting: engine channel closed");
                    return;
                }
            }
        }
    })
}

/// Return `true` while the process `pid` still exists.
///
/// Platform notes:
///
/// * **Windows** — `OpenProcess(PROCESS_SYNCHRONIZE)` + a zero-timeout
///   `WaitForSingleObject`, exactly as C++ does. A process handle becomes
///   signalled on exit, so `WAIT_TIMEOUT` means "still running". If the handle
///   cannot be opened at all the process is treated as gone, which matches the
///   C++ code path (it does not check `OpenProcess` for failure and a null
///   handle makes `WaitForSingleObject` return `WAIT_FAILED`, i.e. not
///   `WAIT_TIMEOUT`).
/// * **Unix** — `kill(pid, 0)`. This is more portable than the C++
///   `access("/proc/<pid>", F_OK)` check (which is Linux-only) and matches the
///   `sysctl` variant used on macOS. `EPERM` means the process exists but is
///   owned by another user, so it counts as alive.
pub fn is_process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};

        // SAFETY: `OpenProcess` is a plain Win32 call; the handle is closed on
        // every path below and never escapes this block.
        unsafe {
            let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if handle.is_null() {
                // Either the PID does not exist or we lack SYNCHRONIZE rights.
                // C++ has the same blind spot; erring toward "gone" keeps
                // `--stop-with-process` from hanging forever on a stale PID.
                return false;
            }
            let wait = windows_sys::Win32::System::Threading::WaitForSingleObject(handle, 0);
            CloseHandle(handle);
            wait == WAIT_TIMEOUT
        }
    }

    #[cfg(unix)]
    {
        // SAFETY: `kill` with signal 0 performs the permission/existence check
        // without delivering a signal.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if rc == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
        // No way to introspect processes — report "alive" so the watcher never
        // shuts the engine down spuriously on an unsupported platform.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn current_process_is_alive() {
        assert!(is_process_alive(std::process::id()));
    }

    #[test]
    fn implausible_pid_is_not_alive() {
        // PID 0 is the idle/swapper pseudo-process on both platforms and can
        // never be opened as a normal process, and u32::MAX is far above every
        // real PID range.
        assert!(!is_process_alive(u32::MAX));
    }

    #[tokio::test(start_paused = true)]
    async fn timed_halt_sends_graceful_halt_after_duration() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = spawn_timed_halt(tx, Duration::from_secs(30), false);

        // Nothing before the deadline.
        assert!(rx.try_recv().is_err());

        tokio::time::advance(Duration::from_secs(31)).await;
        handle.await.unwrap();

        match rx.try_recv() {
            Ok(EngineCommand::HaltAll { reason }) => {
                assert_eq!(reason, HaltReason::ShutdownSignal);
            }
            other => panic!("expected HaltAll, got {:?}", other.is_ok()),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn timed_halt_honours_force_flag() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = spawn_timed_halt(tx, Duration::from_secs(5), true);

        tokio::time::advance(Duration::from_secs(6)).await;
        handle.await.unwrap();

        assert!(matches!(
            rx.try_recv(),
            Ok(EngineCommand::ForceHaltAll { .. })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn timed_halt_exits_when_engine_channel_closes() {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = spawn_timed_halt(tx, Duration::from_secs(3600), false);

        drop(rx);
        // Must finish without waiting out the hour.
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn process_watch_halts_immediately_for_dead_pid() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = spawn_process_watch(tx, u32::MAX, false);
        handle.await.unwrap();

        assert!(matches!(rx.try_recv(), Ok(EngineCommand::HaltAll { .. })));
    }

    #[tokio::test(start_paused = true)]
    async fn process_watch_keeps_running_while_process_lives() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = spawn_process_watch(tx, std::process::id(), false);

        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;

        assert!(rx.try_recv().is_err(), "must not halt while alive");
        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn process_watch_exits_when_engine_channel_closes() {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = spawn_process_watch(tx, std::process::id(), false);

        tokio::task::yield_now().await;
        drop(rx);
        handle.await.unwrap();
    }
}
