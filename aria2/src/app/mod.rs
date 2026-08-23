//! Top-level application runtime for aria2-rust CLI.
//!
//! `App` encapsulates the complete download lifecycle:
//!
//! 1. **Configuration** — `ConfigManager` with 4-source option merging
//! 2. **Engine** — `DownloadEngine` event loop for command execution
//! 3. **Request management** — `RequestGroupMan` for task lifecycle
//! 4. **UI** — Progress display, status panel, and logging
//!
//! # Example
//!
//! ```rust,no_run
//! use aria2::app::cli::CliArgs;
//! use aria2::app::App;
//! use clap::Parser;
//!
//! #[tokio::main]
//! async fn main() {
//!     let cli = CliArgs::parse();
//!     let exit_code = App::new().run(cli).await;
//!     std::process::exit(exit_code);
//! }
//! ```

use colored::Colorize;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use aria2_core::config::ConfigManager;
use aria2_core::init_logging;
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_core::validation::protocol_detector::DetectedInput;
use tracing::{info, warn};

// Daemon support (module declared in lib.rs as `pub mod daemon;`)
use crate::daemon::{DaemonConfig, Daemonizer, PidFileGuard, PidFileManager, is_daemon_child};

// Sub-modules
pub mod cli;
use cli::CliArgs;
mod config;
mod engine;
mod metadata;
pub mod rpc_backend;
// Public so integration tests can exercise the core → RPC notification bridge
// (`rpc::CoreEventBridge`) without spinning up a real RPC server.
pub mod rpc;
mod session;
#[cfg(test)]
mod tests;

/// Top-level application runtime for aria2-rust CLI.
pub struct App {
    pub config: Arc<RwLock<ConfigManager>>,
    engine: Arc<Mutex<Option<aria2_core::engine::download_engine::DownloadEngine>>>,
    request_man: Arc<RequestGroupMan>,
    detected_inputs: Vec<DetectedInput>,
    /// Whether the user explicitly supplied the generic `--timeout` option.
    /// The registry keeps the HTTP/FTP-compatible default at 60 seconds, but
    /// an omitted generic timeout must not impose a BT-wide inactivity halt.
    explicit_timeout: bool,
}

fn console_progress_enabled(show_console_readout: bool, quiet: bool) -> bool {
    show_console_readout && !quiet
}

impl App {
    /// Create a new `App` instance with default configuration.
    pub fn new() -> Self {
        let config = Arc::new(RwLock::new(
            ConfigManager::new_with_identity_without_config(
                crate::identity::DEFAULT_USER_AGENT,
                crate::identity::DEFAULT_PEER_AGENT,
            ),
        ));
        let request_man = Arc::new(RequestGroupMan::new());

        Self {
            config,
            engine: Arc::new(Mutex::new(None)),
            request_man,
            detected_inputs: Vec::new(),
            explicit_timeout: false,
        }
    }

    /// Run the complete application lifecycle.
    ///
    /// This is the main entry point that:
    /// 1. Applies `--no-color` / TTY detection (color control)
    /// 2. Loads config from env → file → CLI args (4-source merge)
    /// 3. **Handles daemon mode if `--daemon` is specified**
    /// 4. Initializes the download engine
    /// 5. **Restores session from input-file (if configured)**
    /// 6. Adds download tasks from positional URIs
    /// 7. Runs the engine event loop
    /// 8. **Saves session on shutdown (if configured)**
    ///
    /// `--help[=TAG|KEYWORD]` / `--version` are handled before `run` is called.
    ///
    /// Returns exit code: `0` = success, `1` = error.
    pub async fn run(&mut self, cli: CliArgs) -> i32 {
        // Apply --no-color flag + TTY detection: disable colored output when
        // the user requests it OR when stdout is not a terminal (e.g. piped).
        if cli.no_color.unwrap_or(false) || !std::io::stdout().is_terminal() {
            colored::control::set_override(false);
        }

        // verbose is handled via log-level config option

        // Handle --no-conf and --conf-path (matching original aria2 option_processing.cc):
        // - --no-conf: skip config file loading entirely
        // - --conf-path: use explicit path (error if not found)
        // - neither: use default ~/.aria2/aria2.conf
        let no_conf = cli.general.no_conf.unwrap_or(false);
        let conf_path = if no_conf {
            None
        } else {
            cli.general
                .conf_path
                .as_ref()
                .and_then(|p| p.to_str())
                .map(|s| s.to_string())
        };

        if let Err(e) = self
            .load_startup_config(no_conf, conf_path.as_deref())
            .await
        {
            eprintln!("Failed to load config file: {}", e);
            return 1;
        }

        if let Err(e) = self.load_cli_args(cli).await {
            eprintln!("{}", format!("Argument parsing error: {}", e).red());
            return 1;
        }

        if self.get_opt_bool("show-files").await.unwrap_or(false) {
            return match metadata::show_files(&self.detected_inputs) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("Failed to show metadata: {error}");
                    1
                }
            };
        }

        if !self.get_opt_bool("enable-color").await.unwrap_or(true) {
            colored::control::set_override(false);
        }

        // Check daemon mode early - must happen before any output
        let daemon_child = is_daemon_child();
        let daemon_mode = daemon_child || self.get_opt_bool("daemon").await.unwrap_or(false);
        let pid_file = self.get_opt_str("pid-file").await.map(PathBuf::from);

        if daemon_mode && !daemon_child {
            // Check if daemon is already running
            if let Some(ref path) = pid_file {
                let pid_mgr = PidFileManager::new(path.clone());
                if let Some(existing_pid) = pid_mgr.check_existing_for_current_executable() {
                    eprintln!(
                        "{}",
                        format!("Daemon already running with PID: {}", existing_pid).yellow()
                    );
                    return 0;
                }
            }

            // ===================================================================
            // CRITICAL ORDERING CONSTRAINT — daemonize must run BEFORE the tokio
            // runtime's I/O driver (reactor) is initialized and before the engine
            // binds any sockets. See `Daemonizer::daemonize` docs in daemon.rs.
            //
            // Current guarantees at this point:
            //  * main.rs `#[tokio::main]` created the runtime object, but the
            //    reactor is created LAZILY on the first socket/timer registration.
            //    Up to here only synchronous `std::fs` reads and in-memory
            //    RwLock/Mutex ops have run — no fd has been registered with the
            //    runtime, so `close_file_descriptors_unix` closing fds 3..max_fd
            //    will NOT destroy a live reactor fd (epoll/eventfd/socket).
            //  * `initialize_engine()` (called later in this fn) binds RPC server
            //    sockets and spawns engine tasks. daemonize MUST stay before it.
            //
            // Do NOT move this block after `initialize_engine()`, into a tokio
            // task, or after the RPC server binds — the daemon child would then
            // inherit-and-close the reactor's epoll/eventfd and crash on first I/O.
            // ===================================================================
            // Assertion-style guard: if the engine already exists we are too late.
            // Its sockets/files are registered with the runtime, so fd-closing in
            // the daemon child would corrupt the reactor. Fails loudly in debug
            // builds, warns in release.
            if self.engine.lock().await.is_some() {
                warn!(
                    "daemonize() called after engine initialization — \
                     inherited-fd closing in daemon mode may corrupt the tokio runtime"
                );
                debug_assert!(
                    false,
                    "daemonize() must be called before initialize_engine()"
                );
            }

            // Perform daemonization
            // Note: Do NOT pass log_path to stdout_file/stderr_file.
            // File logging is handled by init_logging() below using rolling appender.
            // Passing log_path here would create duplicate log files.
            let daemon_config = DaemonConfig {
                pid_file: pid_file.clone(),
                stdout_file: None,
                stderr_file: None,
                chdir_to_root: false,
                close_fds: true,
            };

            let daemonizer = Daemonizer::new(daemon_config);
            if let Err(e) = daemonizer.daemonize() {
                eprintln!("{}", format!("Failed to daemonize: {}", e).red());
                return 1;
            }

            // After daemonization, we are in the child process
            // Re-initialize logging for the daemon process
            let log_level = self
                .get_opt_str("log-level")
                .await
                .unwrap_or_else(|| "info".to_string());
            let console_log_level = if self.get_opt_bool("quiet").await.unwrap_or(false) {
                "error".to_string()
            } else {
                self.get_opt_str("console-log-level")
                    .await
                    .unwrap_or_else(|| "notice".to_string())
            };
            let log_path = self.get_opt_str("log").await;
            let log_backup_count = self.get_opt_i64("log-backup-count").await.unwrap_or(5) as usize;
            let log_max_size = self
                .get_opt_i64("log-max-size")
                .await
                .filter(|&v| v > 0)
                .map(|v| v as u64);
            let log_max_files = self.get_opt_i64("log-max-files").await.map(|v| v as usize);
            init_logging(
                &log_level,
                &console_log_level,
                log_path.as_deref(),
                log_backup_count,
                log_max_size,
                log_max_files,
            );

            info!("Daemon started successfully");
        }

        // `daemonize()` only returns in the daemon child. The parent exits
        // inside the platform implementation, so this guard is created with
        // the actual daemon PID and remains alive for App::run.
        let _pid_file_guard = if daemon_mode {
            pid_file.map(|path| PidFileGuard::new(path, std::process::id()))
        } else {
            None
        };

        // In daemon mode, logging was already re-initialized after daemonization above.
        if !daemon_mode {
            let log_level = self
                .get_opt_str("log-level")
                .await
                .unwrap_or_else(|| "info".to_string());
            let console_log_level = if self.get_opt_bool("quiet").await.unwrap_or(false) {
                "error".to_string()
            } else {
                self.get_opt_str("console-log-level")
                    .await
                    .unwrap_or_else(|| "notice".to_string())
            };
            let log_path = self.get_opt_str("log").await;
            let log_backup_count = self.get_opt_i64("log-backup-count").await.unwrap_or(5) as usize;
            let log_max_size = self
                .get_opt_i64("log-max-size")
                .await
                .filter(|&v| v > 0)
                .map(|v| v as u64);
            let log_max_files = self.get_opt_i64("log-max-files").await.map(|v| v as usize);
            init_logging(
                &log_level,
                &console_log_level,
                log_path.as_deref(),
                log_backup_count,
                log_max_size,
                log_max_files,
            );
        }

        let quiet = self.get_opt_bool("quiet").await.unwrap_or(false);
        let output_to_stderr = self.get_opt_bool("stderr").await.unwrap_or(false);
        if !quiet {
            self.print_banner(output_to_stderr);
        }

        // Apply engine-level options from config (CLI/file/env) BEFORE tasks
        // are added. Zero is the explicit unlimited value, so it must also
        // override the manager's default of five.
        if let Some(max) = self.get_opt_i64("max-concurrent-downloads").await
            && let Ok(max) = u32::try_from(max)
        {
            self.request_man.set_max_concurrent(max);
            info!(
                "Max concurrent downloads set to {} (from config)",
                if max == 0 {
                    "unlimited".to_string()
                } else {
                    max.to_string()
                }
            );
        }

        // Initialize engine (must be before session restore)
        self.initialize_engine().await;

        // Step 4: Restore incomplete downloads from session file
        match self.restore_session().await {
            Ok(count) => {
                if count > 0 {
                    info!("Successfully restored {} download tasks", count);
                }
            }
            Err(e) => {
                warn!("Session restore failed (will continue): {}", e);
                // Restore failure doesn't block execution, just log warning
            }
        }

        // Check if there are any inputs (restored tasks or CLI URIs). Keep the
        // manager guard scoped to this synchronous snapshot: `run` continues
        // through RPC startup and the engine lifetime, so retaining it here
        // would starve the first RPC write lock indefinitely.
        let has_restored_tasks = { self.request_man.count() > 0 };

        // In daemon mode, we need RPC enabled to control the daemon
        let rpc_enabled = self.get_opt_bool("enable-rpc").await.unwrap_or(false);

        if !has_restored_tasks && self.detected_inputs.is_empty() {
            if rpc_enabled {
                info!("Starting in RPC-only mode (no initial downloads)");
            } else if daemon_mode {
                warn!("Daemon mode requires --enable-rpc when no downloads are specified");
                info!("Starting daemon with RPC server for remote control");
            } else {
                eprintln!(
                    "{}",
                    "Error: Please provide a download URI or torrent file path, or use --input-file to resume previous downloads".red()
                );
                return 1;
            }
        }

        // Step 5: Add CLI-specified download tasks
        if !self.detected_inputs.is_empty() {
            match self.add_downloads().await {
                Ok(gids) => {
                    info!("Added {} download tasks", gids.len());
                    if !quiet {
                        for gid in &gids {
                            let line =
                                format!("  {} Task #{}\n", "#".cyan(), gid.to_string().yellow());
                            if output_to_stderr {
                                eprint!("{}", line);
                            } else {
                                print!("{}", line);
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("{}", format!("Failed to add task: {}", e).red());
                    return 1;
                }
            }
        } else if has_restored_tasks {
            info!("Using restored download tasks only");
        }

        if !quiet {
            if output_to_stderr {
                eprintln!();
            } else {
                println!();
            }
        }

        // Step 6: Start RPC server (if enabled)
        let rpc_handle = if rpc_enabled {
            // Extract shared state from the engine before run() consumes it
            let (group_man, engine_cmd_tx) = {
                let engine_lock = self.engine.lock().await;
                let engine_ref = engine_lock.as_ref().expect("engine should be initialized");
                (self.request_man.clone(), engine_ref.engine_command_sender())
            };
            match self.start_rpc_server(group_man, engine_cmd_tx).await {
                Ok(handle) => Some(handle),
                Err(e) => {
                    eprintln!("Failed to start RPC server: {}", e);
                    return 1;
                }
            }
        } else {
            None
        };

        // Step 7: Run engine with the configured console readout. Redirected
        // stdout is still a valid consumer of plain progress lines, as with
        // aria2_original and Scoop's PowerShell pipeline.
        let show_progress = console_progress_enabled(
            self.get_opt_bool("show-console-readout")
                .await
                .unwrap_or(true),
            self.get_opt_bool("quiet").await.unwrap_or(false),
        );
        let run_result = self.run_engine(rpc_enabled, show_progress).await;

        // Step 8: Shutdown RPC server
        if let Some(handle) = rpc_handle {
            handle.abort();
            info!("RPC server shut down");
        }

        // Step 9: Save session on shutdown
        if let Err(e) = self.save_session_on_shutdown().await {
            warn!("Failed to save session on shutdown: {}", e);
            // Save failure doesn't affect exit code
        }

        let stopped_results = self.request_man.get_stopped_results(0, usize::MAX);
        if !quiet {
            let summary = crate::ui::progress_bar::render_final_summary(&stopped_results);
            if !summary.is_empty() {
                if output_to_stderr {
                    eprint!("{}", summary);
                } else {
                    print!("{}", summary);
                }
            }
        }

        match run_result {
            Ok(())
                if stopped_results
                    .iter()
                    .all(|result| !result_is_failure(result)) =>
            {
                0
            }
            Ok(()) => {
                let failed = stopped_results
                    .iter()
                    .filter(|result| result_is_failure(result))
                    .count();
                eprintln!("Download failed: {} task(s) failed", failed);
                1
            }
            Err(e) => {
                eprintln!("{}", format!("Download failed: {}", e).red());
                1
            }
        }
    }
}

fn result_is_failure(result: &aria2_core::request::request_group::DownloadResult) -> bool {
    !result.code.is_success() && !result.code.is_user_stopped() && !result.code.is_resumable()
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
