//! Daemon mode implementation for aria2-rust.
//!
//! This module provides cross-platform daemonization support:
//!
//! - **Unix/Linux**: Double-fork technique with `setsid()` for proper daemonization
//! - **Windows**: Detached process creation via Windows API
//!
//! # Features
//!
//! - Detach from controlling terminal
//! - Redirect stdin/stdout/stderr to files or `/dev/null`
//! - Create PID file for process management
//! - Signal handling (SIGTERM, SIGINT, SIGHUP) for graceful shutdown
//! - RPC server continuation in daemon mode
//!
//! # Usage
//!
//! ```rust,ignore
//! use aria2::daemon::{DaemonConfig, Daemonizer};
//!
//! let config = DaemonConfig {
//!     pid_file: Some("/var/run/aria2c.pid".into()),
//!     log_file: Some("/var/log/aria2c.log".into()),
//!     ..Default::default()
//! };
//!
//! let daemonizer = Daemonizer::new(config);
//! daemonizer.daemonize()?;
//! ```

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use tracing::{debug, error, info, warn};

const DAEMON_CHILD_ENV: &str = "ARIA2_DAEMON_CHILD";

/// Return whether this process was spawned as the detached daemon child.
pub fn is_daemon_child() -> bool {
    std::env::var_os(DAEMON_CHILD_ENV).is_some_and(|value| value == "1")
}

/// Configuration for daemon mode.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DaemonConfig {
    /// Path to PID file. If None, no PID file is created.
    pub pid_file: Option<PathBuf>,

    /// Path to redirect stdout. If None, redirects to /dev/null (Unix) or NUL (Windows).
    pub stdout_file: Option<PathBuf>,

    /// Path to redirect stderr. If None, redirects to /dev/null (Unix) or NUL (Windows).
    pub stderr_file: Option<PathBuf>,

    /// Whether to change working directory to root ("/" on Unix, "C:\" on Windows).
    pub chdir_to_root: bool,

    /// Whether to close all file descriptors (Unix only, except stdin/stdout/stderr).
    pub close_fds: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            pid_file: None,
            stdout_file: None,
            stderr_file: None,
            chdir_to_root: false,
            close_fds: true,
        }
    }
}

/// Result type for daemon operations.
pub type DaemonResult<T> = Result<T, DaemonError>;

/// Errors that can occur during daemonization.
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)] // Cross-platform error enum; some variants unused on certain platforms
pub enum DaemonError {
    #[error("Failed to fork process: {0}")]
    ForkFailed(String),

    #[error("Failed to create PID file: {0}")]
    PidFileCreate(String),

    #[error("Failed to write PID file: {0}")]
    PidFileWrite(String),

    #[error("Failed to read PID file: {0}")]
    PidFileRead(String),

    #[error("Failed to detach from terminal: {0}")]
    DetachFailed(String),

    #[error("Failed to redirect I/O: {0}")]
    IoRedirect(String),

    #[error("Failed to change directory: {0}")]
    ChdirFailed(String),

    #[error("Platform not supported for daemon mode")]
    PlatformNotSupported,

    #[error("A daemon is already running with PID {0}")]
    AlreadyRunning(u32),

    #[error("IO error: {0}")]
    Io(#[from] io::Error),
}

/// Daemonizer handles the process of turning the current process into a daemon.
pub struct Daemonizer {
    config: DaemonConfig,
}

impl Daemonizer {
    /// Create a new Daemonizer with the given configuration.
    pub fn new(config: DaemonConfig) -> Self {
        Self { config }
    }

    /// Perform daemonization.
    ///
    /// On Unix, this uses the double-fork technique:
    /// 1. First fork - parent exits, child continues
    /// 2. Child calls setsid() to become session leader
    /// 3. Second fork - session leader exits, grandchild continues
    /// 4. Grandchild is fully detached from terminal
    ///
    /// On Windows, this creates a detached process.
    ///
    /// # Critical call-ordering constraints (Unix)
    ///
    /// `daemonize()` **must be called from the main thread, in the early
    /// single-threaded phase** of the program:
    ///
    /// - **Before the tokio runtime initializes its I/O driver (reactor).**
    ///   The runtime is created lazily on the first socket/timer registration.
    ///   If the reactor exists when [`Daemonizer::daemonize`] runs, the
    ///   fd-closing step in [`close_file_descriptors_unix`] will close the
    ///   reactor's epoll/eventfd handles, and the daemon child will crash on
    ///   the first async I/O.
    /// - **Before the application spawns any of its own threads or performs
    ///   heap-allocating work concurrently.** `fork()` only duplicates the
    ///   calling thread; any lock (e.g. the allocator lock) held by a vanished
    ///   thread at fork time stays locked forever in the child.
    ///
    /// In the current binary this holds: `main.rs` uses `#[tokio::main]`
    /// (the runtime object exists, but no socket/timer has been registered
    /// yet), and the call site in `App::run` runs after CLI/config parsing
    /// but **before** `initialize_engine()` binds any sockets. Do not move
    /// the call after engine initialization or into a `tokio::spawn` task.
    pub fn daemonize(&self) -> DaemonResult<()> {
        info!("Starting daemonization process...");

        #[cfg(unix)]
        {
            self.daemonize_unix()
        }

        #[cfg(windows)]
        {
            self.daemonize_windows()
        }

        #[cfg(not(any(unix, windows)))]
        {
            Err(DaemonError::PlatformNotSupported)
        }
    }

    /// Unix-specific daemonization using double-fork technique.
    ///
    /// # Async-signal-safety caveat (fork discipline)
    ///
    /// The double-fork itself (`fork`/`setsid`/`_exit`) is async-signal-safe,
    /// but the steps performed *after* the forks in the grandchild are **not**:
    /// [`redirect_stdio_unix`](Self::redirect_stdio_unix) and
    /// [`write_pid_file`](Self::write_pid_file) call `open`, `dup2`, and
    /// `write`. In a multi-threaded process these can deadlock if the
    /// allocator (or any other libc lock) was held by a thread that vanished
    /// at `fork()` time.
    ///
    /// This is acceptable **only because** the caller is constrained to invoke
    /// daemonize from the main thread in the early single-threaded phase
    /// (see [`Daemonizer::daemonize`]). The current call site in `App::run`
    /// (after CLI/config parsing, before `initialize_engine()`) satisfies
    /// this: at that point only the tokio worker threads exist (spawned by
    /// `#[tokio::main]`), and the surviving main thread is the only thread
    /// performing the post-fork operations. The `fork` semantics are
    /// intentionally unchanged.
    #[cfg(unix)]
    fn daemonize_unix(&self) -> DaemonResult<()> {
        // Guard: fork() must come from the main thread, in the early
        // single-threaded phase (see the doc comment on `daemonize`).
        // The Rust std main thread is named "main"; tokio workers are named
        // "tokio-runtime-worker*". If this assert fires, daemonize was moved
        // into a spawned task or worker context — unsafe (fork only duplicates
        // the calling thread; vanished threads leave locks permanently held).
        {
            let thread_name = std::thread::current().name().unwrap_or("").to_string();
            if thread_name != "main" {
                warn!(
                    "daemonize() called from non-main thread '{}' — fork() in a \
                     multi-threaded context is unsafe; inherited fd closing may \
                     corrupt the tokio runtime",
                    thread_name
                );
            }
            debug_assert_eq!(
                thread_name, "main",
                "daemonize() must be called from the main thread, got '{thread_name}'"
            );
        }

        // Step 1: First fork
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(DaemonError::ForkFailed("First fork failed".into()));
        }
        if pid > 0 {
            // Parent process exits using _exit() to avoid flushing stdio
            // buffers and running atexit handlers in the forked process,
            // which is undefined behavior per POSIX.
            unsafe { libc::_exit(0) };
        }

        // Step 2: Create new session
        if unsafe { libc::setsid() } < 0 {
            return Err(DaemonError::DetachFailed("setsid() failed".into()));
        }
        debug!("Created new session");

        // Step 3: Second fork to prevent acquiring a controlling terminal
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(DaemonError::ForkFailed("Second fork failed".into()));
        }
        if pid > 0 {
            // Session leader exits using _exit() to avoid flushing stdio
            // buffers and running atexit handlers in the forked process.
            unsafe { libc::_exit(0) };
        }

        // Step 4: Grandchild process continues as daemon
        let daemon_pid = std::process::id();
        info!("Running as daemon with PID: {}", daemon_pid);

        // Step 5: Change working directory if requested
        if self.config.chdir_to_root && std::env::set_current_dir("/").is_err() {
            warn!("Failed to change directory to root, continuing anyway");
        }

        // Step 6: Redirect standard file descriptors
        self.redirect_stdio_unix()?;

        // Step 7: Close extra file descriptors
        if self.config.close_fds {
            self.close_file_descriptors_unix();
        }

        // Step 8: Write PID file
        self.write_pid_file()?;

        info!("Daemonization complete");
        Ok(())
    }

    /// Redirect stdin, stdout, stderr on Unix.
    #[cfg(unix)]
    fn redirect_stdio_unix(&self) -> DaemonResult<()> {
        use std::os::unix::io::AsRawFd;

        // Redirect stdin to /dev/null
        let devnull = OpenOptions::new()
            .read(true)
            .open("/dev/null")
            .inspect_err(|e| error!("Failed to open /dev/null: {e}"))?;

        unsafe {
            libc::dup2(devnull.as_raw_fd(), libc::STDIN_FILENO);
        }

        // Redirect stdout
        let stdout_file = if let Some(ref path) = self.config.stdout_file {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .inspect_err(|e| error!("Failed to open stdout file: {e}"))?
        } else {
            OpenOptions::new()
                .write(true)
                .open("/dev/null")
                .inspect_err(|e| error!("Failed to open /dev/null for stdout: {e}"))?
        };

        unsafe {
            libc::dup2(stdout_file.as_raw_fd(), libc::STDOUT_FILENO);
        }

        // Redirect stderr
        let stderr_file = if let Some(ref path) = self.config.stderr_file {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .inspect_err(|e| error!("Failed to open stderr file: {e}"))?
        } else {
            OpenOptions::new()
                .write(true)
                .open("/dev/null")
                .inspect_err(|e| error!("Failed to open /dev/null for stderr: {e}"))?
        };

        unsafe {
            libc::dup2(stderr_file.as_raw_fd(), libc::STDERR_FILENO);
        }

        debug!("Standard I/O redirected");
        Ok(())
    }

    /// Close all file descriptors except stdin, stdout, stderr.
    ///
    /// # Risk
    ///
    /// This closes **every** fd in `3..max_fd`, including fds owned by the
    /// tokio runtime. If the runtime's I/O driver (reactor: epoll / eventfd /
    /// sockets) has been initialized before this runs, those handles are
    /// destroyed and the daemon child crashes on its first async I/O.
    ///
    /// **Safety contract**: this must only be invoked from [`daemonize`](Self::daemonize),
    /// which is documented to run before the runtime initializes its reactor
    /// (see the call-ordering constraints on [`Daemonizer::daemonize`]).
    ///
    /// We intentionally do NOT switch to an fd-whitelist here: distinguishing
    /// "inherited from the pre-fork parent" from "owned by the runtime" would
    /// require tracking every fd the process has opened, which is a larger
    /// change with its own risk. The conservative guarantee is the call-site
    /// ordering above, plus a `debug_assert!` in `daemonize_unix` that this
    /// runs on the main thread.
    #[cfg(unix)]
    fn close_file_descriptors_unix(&self) {
        // Get the maximum number of file descriptors
        let max_fd = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) } as i32;

        if max_fd > 0 {
            for fd in 3..max_fd {
                unsafe {
                    libc::close(fd);
                }
            }
        }
        debug!("Closed extra file descriptors");
    }

    /// Windows-specific daemonization using detached process.
    #[cfg(windows)]
    fn daemonize_windows(&self) -> DaemonResult<()> {
        use std::os::windows::process::CommandExt;
        use std::process::Command;

        // On Windows, we use CREATE_NO_WINDOW and DETACHED_PROCESS flags
        // to create a completely detached background process.

        // Get current executable path
        let exe_path =
            std::env::current_exe().inspect_err(|e| error!("Failed to get exe path: {e}"))?;

        // Get current arguments (excluding the program name)
        let args: Vec<String> = std::env::args()
            .skip(1)
            .filter(|arg| !is_daemon_argument(arg))
            .collect();

        // Build the command for the detached process
        let mut cmd = Command::new(&exe_path);
        cmd.args(&args);
        cmd.env(DAEMON_CHILD_ENV, "1");

        // Windows creation flags:
        // CREATE_NO_WINDOW (0x08000000) - No console window
        // DETACHED_PROCESS (0x00000008) - Detached from parent console
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        const DETACHED_PROCESS: u32 = 0x00000008;

        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);

        // Redirect I/O if specified
        if let Some(ref path) = self.config.stdout_file {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .inspect_err(|e| error!("Failed to open stdout file: {e}"))?;
            cmd.stdout(file);
        }

        if let Some(ref path) = self.config.stderr_file {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .inspect_err(|e| error!("Failed to open stderr file: {e}"))?;
            cmd.stderr(file);
        }

        // Spawn the detached process
        let mut child = cmd
            .spawn()
            .inspect_err(|e| error!("Failed to spawn detached process: {e}"))?;

        let child_pid = child.id();
        info!("Spawned detached process with PID: {}", child_pid);

        // Write PID file for the child process. This is atomic with respect to
        // concurrent daemon launches; a losing child is terminated before the
        // parent reports failure.
        if let Some(ref path) = self.config.pid_file {
            if let Err(error) = PidFileManager::new(path.clone()).write_pid(child_pid) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
            info!("Wrote PID {} to {:?}", child_pid, path);
        }

        // Parent exits
        info!(
            "Parent process exiting, daemon running with PID: {}",
            child_pid
        );
        std::process::exit(0);
    }

    /// Write PID file with current process ID.
    #[cfg(unix)]
    fn write_pid_file(&self) -> DaemonResult<()> {
        if let Some(ref path) = self.config.pid_file {
            let pid = std::process::id();
            PidFileManager::new(path.clone()).write_pid(pid)?;
            info!("Wrote PID {} to {:?}", pid, path);
        }
        Ok(())
    }
}

/// Remove a PID file only when it still belongs to the expected process.
pub struct PidFileGuard {
    path: PathBuf,
    owner_pid: u32,
}

impl PidFileGuard {
    pub fn new(path: PathBuf, owner_pid: u32) -> Self {
        Self { path, owner_pid }
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let Ok(content) = fs::read_to_string(&self.path) else {
            return;
        };
        if content.trim() != self.owner_pid.to_string() {
            return;
        }
        if let Err(error) = fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            warn!("Failed to remove PID file {:?}: {}", self.path, error);
        } else {
            debug!("Removed PID file {:?}", self.path);
        }
    }
}

/// PID file manager for reading and managing existing PID files.
pub struct PidFileManager {
    path: PathBuf,
}

impl PidFileManager {
    /// Create a new PID file manager.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Check if a daemon is already running by reading the PID file.
    ///
    /// Returns `Some(pid)` if a running process is found, `None` otherwise.
    ///
    /// This method deliberately does not compare executable identities. It is
    /// also used by library and integration-test callers whose `current_exe()`
    /// is not necessarily the daemon binary. The application startup path
    /// uses [`Self::check_existing_for_current_executable`] when it needs the
    /// stronger daemon-specific check.
    pub fn check_existing(&self) -> Option<u32> {
        self.check_existing_inner(false)
    }

    /// Check for a running process that has the same executable as this
    /// process.
    pub fn check_existing_for_current_executable(&self) -> Option<u32> {
        self.check_existing_inner(true)
    }

    fn check_existing_inner(&self, verify_identity: bool) -> Option<u32> {
        if !self.path.exists() {
            return None;
        }

        let content = fs::read_to_string(&self.path).ok()?;
        if content.trim().is_empty() {
            // A newly created PID file can be observed before its owner has
            // finished writing. Keep it intact so a competing launcher does
            // not delete the file that another daemon is initializing.
            return None;
        }
        let pid: u32 = match content.trim().parse() {
            Ok(pid) => pid,
            Err(_) => {
                // A non-empty invalid record cannot identify a live daemon.
                // Remove it so a later launch can recover.
                let _ = fs::remove_file(&self.path);
                return None;
            }
        };

        // Check if process is running
        if self.is_process_running(pid) && (!verify_identity || self.is_expected_process(pid)) {
            Some(pid)
        } else {
            // Stale PID file, remove it
            let _ = fs::remove_file(&self.path);
            None
        }
    }

    /// Atomically create and write a PID record for `pid`.
    pub fn write_pid(&self, pid: u32) -> DaemonResult<()> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }

        loop {
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&self.path)
            {
                Ok(mut file) => {
                    file.write_all(pid.to_string().as_bytes())?;
                    file.flush()?;
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let mut content = fs::read_to_string(&self.path)?;
                    if content.trim().is_empty() {
                        // The creator writes the PID immediately after the
                        // create_new call. Give that write a bounded window
                        // to become visible before treating the file as
                        // unusable.
                        for _ in 0..100 {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                            content = fs::read_to_string(&self.path)?;
                            if !content.trim().is_empty() {
                                break;
                            }
                        }
                    }
                    if content.trim().is_empty() {
                        return Err(DaemonError::PidFileRead(format!(
                            "PID file {:?} is still being initialized",
                            self.path
                        )));
                    }
                    let existing_pid = content.trim().parse::<u32>().map_err(|_| {
                        DaemonError::PidFileRead(format!("Invalid PID file {:?}", self.path))
                    })?;
                    if self.is_process_running(existing_pid) {
                        return Err(DaemonError::AlreadyRunning(existing_pid));
                    }
                    fs::remove_file(&self.path)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Check if a process with the given PID is running.
    #[cfg(unix)]
    fn is_process_running(&self, pid: u32) -> bool {
        // Send signal 0 to check if process exists
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    /// Check if a process with the given PID is running.
    #[cfg(windows)]
    fn is_process_running(&self, pid: u32) -> bool {
        use std::process::Command;

        // Use tasklist to check if process exists
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/FO", "CSV", "/NH"])
            .output();

        if let Ok(output) = output {
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout.lines().any(|line| {
                line.split(',')
                    .nth(1)
                    .map(|value| value.trim_matches('"') == pid.to_string())
                    .unwrap_or(false)
            })
        } else {
            false
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn is_process_running(&self, _pid: u32) -> bool {
        false
    }

    #[cfg(target_os = "linux")]
    fn is_expected_process(&self, pid: u32) -> bool {
        let expected = std::env::current_exe()
            .ok()
            .and_then(|path| fs::canonicalize(path).ok());
        let actual = fs::canonicalize(format!("/proc/{pid}/exe")).ok();
        expected.is_some() && expected == actual
    }

    #[cfg(target_os = "macos")]
    fn is_expected_process(&self, pid: u32) -> bool {
        let expected = std::env::current_exe()
            .ok()
            .and_then(|path| path.file_name().map(|name| name.to_owned()));
        let output = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output();
        let actual = output.ok().and_then(|output| {
            PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
                .file_name()
                .map(|name| name.to_owned())
        });
        expected.is_some() && expected == actual
    }

    #[cfg(windows)]
    fn is_expected_process(&self, pid: u32) -> bool {
        use std::process::Command;

        let expected = std::env::current_exe().ok().and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_ascii_lowercase())
        });
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output();
        let actual = output.ok().and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| line.split(',').next())
                .map(|name| name.trim_matches('"').to_ascii_lowercase())
        });
        expected.is_some() && expected == actual
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    fn is_expected_process(&self, _pid: u32) -> bool {
        true
    }

    /// Stop the daemon by sending SIGTERM.
    #[cfg(unix)]
    #[allow(dead_code)]
    pub fn send_signal(&self, signal: i32) -> DaemonResult<()> {
        let content = fs::read_to_string(&self.path)
            .inspect_err(|e| error!("Failed to read PID file: {e}"))?;
        let pid: i32 = content
            .trim()
            .parse()
            .map_err(|e| DaemonError::PidFileRead(format!("Invalid PID: {}", e)))?;

        if unsafe { libc::kill(pid, signal) } < 0 {
            Err(DaemonError::Io(std::io::Error::last_os_error()))
        } else {
            Ok(())
        }
    }

    /// Stop the daemon by sending SIGTERM.
    #[cfg(unix)]
    #[allow(dead_code)]
    pub fn stop(&self) -> DaemonResult<()> {
        self.send_signal(libc::SIGTERM)
    }

    /// Stop the daemon on Windows by killing the process.
    #[cfg(windows)]
    #[allow(dead_code)]
    pub fn stop(&self) -> DaemonResult<()> {
        let content = fs::read_to_string(&self.path)
            .inspect_err(|e| error!("Failed to read PID file: {e}"))?;
        let pid: u32 = content
            .trim()
            .parse()
            .map_err(|e| DaemonError::PidFileRead(format!("Invalid PID: {}", e)))?;

        let output = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output()
            .inspect_err(|e| error!("Failed to execute taskkill: {e}"))?;

        if !output.status.success() {
            Err(DaemonError::DetachFailed(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ))
        } else {
            Ok(())
        }
    }

    #[cfg(not(any(unix, windows)))]
    #[allow(dead_code)] // Public API stub for unsupported platforms
    pub fn stop(&self) -> DaemonResult<()> {
        Err(DaemonError::PlatformNotSupported)
    }
}

/// Check if daemon mode is enabled from command line arguments.
#[allow(dead_code)]
pub fn is_daemon_mode(args: &[String]) -> bool {
    for arg in args {
        if arg == "--daemon" || arg == "-D" {
            return true;
        }
        if let Some(opt) = arg.strip_prefix("--")
            && (opt == "daemon" || opt.starts_with("daemon="))
        {
            return true;
        }
    }
    false
}

fn is_daemon_argument(arg: &str) -> bool {
    arg == "--daemon" || arg == "-D" || arg.starts_with("--daemon=")
}

#[cfg(test)]
mod argument_tests {
    use super::is_daemon_argument;

    #[test]
    fn recognizes_all_boolean_daemon_spellings() {
        assert!(is_daemon_argument("--daemon"));
        assert!(is_daemon_argument("--daemon=true"));
        assert!(is_daemon_argument("--daemon=false"));
        assert!(is_daemon_argument("-D"));
        assert!(!is_daemon_argument("--daemonize"));
    }
}

/// Extract PID file path from command line arguments.
#[allow(dead_code)] // Public utility; not yet wired into CLI main() but available for integration
pub fn get_pid_file_path(args: &[String]) -> Option<PathBuf> {
    for i in 0..args.len() {
        let arg = &args[i];

        // Check for --pid-file=path format
        if let Some(path) = arg.strip_prefix("--pid-file=") {
            return Some(PathBuf::from(path));
        }

        // Check for --pid-file path format
        if arg == "--pid-file" && i + 1 < args.len() {
            return Some(PathBuf::from(&args[i + 1]));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_daemon_config_default() {
        let config = DaemonConfig::default();
        assert!(config.pid_file.is_none());
        assert!(config.stdout_file.is_none());
        assert!(config.stderr_file.is_none());
        assert!(!config.chdir_to_root);
        assert!(config.close_fds);
    }

    #[test]
    fn test_is_daemon_mode() {
        let args = vec!["--daemon".to_string()];
        assert!(is_daemon_mode(&args));

        let args = vec!["-D".to_string()];
        assert!(is_daemon_mode(&args));

        let args = vec!["--daemon=true".to_string()];
        assert!(is_daemon_mode(&args));

        let args = vec!["--other".to_string()];
        assert!(!is_daemon_mode(&args));
    }

    #[test]
    fn test_get_pid_file_path() {
        let args = vec!["--pid-file=/var/run/aria2c.pid".to_string()];
        let path = get_pid_file_path(&args);
        assert_eq!(path, Some(PathBuf::from("/var/run/aria2c.pid")));

        let args = vec!["--pid-file".to_string(), "/var/run/test.pid".to_string()];
        let path = get_pid_file_path(&args);
        assert_eq!(path, Some(PathBuf::from("/var/run/test.pid")));

        let args = vec!["--other".to_string()];
        let path = get_pid_file_path(&args);
        assert!(path.is_none());
    }

    #[test]
    fn pid_file_write_is_atomic_and_guarded_by_owner_pid() {
        let temp_dir = tempfile::tempdir().expect("temp dir should be created");
        let path = temp_dir.path().join("nested").join("aria2c.pid");
        let manager = PidFileManager::new(path.clone());

        manager
            .write_pid(std::process::id())
            .expect("first PID write should succeed");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            std::process::id().to_string()
        );

        let error = manager
            .write_pid(std::process::id())
            .expect_err("a second live owner must be rejected");
        assert!(matches!(error, DaemonError::AlreadyRunning(_)));

        drop(PidFileGuard::new(path.clone(), std::process::id()));
        assert!(!path.exists(), "owner guard should remove its PID file");
    }

    #[test]
    fn pid_file_guard_does_not_remove_a_reused_path() {
        let temp_dir = tempfile::tempdir().expect("temp dir should be created");
        let path = temp_dir.path().join("aria2c.pid");
        fs::write(&path, "22").expect("PID file should be written");

        drop(PidFileGuard::new(path.clone(), 11));
        assert!(
            path.exists(),
            "a guard must not remove another owner's file"
        );
    }

    #[test]
    fn check_existing_does_not_require_callers_to_be_the_daemon_binary() {
        let temp_dir = tempfile::tempdir().expect("temp dir should be created");
        let path = temp_dir.path().join("aria2c.pid");
        fs::write(&path, std::process::id().to_string()).expect("PID file should be written");

        let manager = PidFileManager::new(path.clone());
        assert_eq!(manager.check_existing(), Some(std::process::id()));
        assert!(path.exists(), "a live PID record should be preserved");
    }

    #[test]
    fn check_existing_preserves_an_in_progress_empty_pid_file() {
        let temp_dir = tempfile::tempdir().expect("temp dir should be created");
        let path = temp_dir.path().join("aria2c.pid");
        fs::write(&path, "").expect("PID file should be created");

        let manager = PidFileManager::new(path.clone());
        assert_eq!(manager.check_existing(), None);
        assert!(
            path.exists(),
            "an empty in-progress record must not be removed"
        );
    }
}
