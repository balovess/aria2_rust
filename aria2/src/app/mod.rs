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
use crate::daemon::{DaemonConfig, Daemonizer, PidFileManager};

// Sub-modules
pub mod cli;
use cli::CliArgs;
mod config;
mod engine;
mod rpc;
mod session;
#[cfg(test)]
mod tests;

/// Top-level application runtime for aria2-rust CLI.
pub struct App {
    pub config: Arc<RwLock<ConfigManager>>,
    engine: Arc<Mutex<Option<aria2_core::engine::download_engine::DownloadEngine>>>,
    request_man: Arc<RwLock<RequestGroupMan>>,
    detected_inputs: Vec<DetectedInput>,
}

impl App {
    /// Create a new `App` instance with default configuration.
    pub fn new() -> Self {
        let config = Arc::new(RwLock::new(ConfigManager::new()));
        let request_man = Arc::new(RwLock::new(RequestGroupMan::new()));

        Self {
            config,
            engine: Arc::new(Mutex::new(None)),
            request_man,
            detected_inputs: Vec::new(),
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
    /// `--help` / `--version` are handled by clap before `run` is called.
    ///
    /// Returns exit code: `0` = success, `1` = error.
    pub async fn run(&mut self, cli: CliArgs) -> i32 {
        // Apply --no-color flag + TTY detection: disable colored output when
        // the user requests it OR when stdout is not a terminal (e.g. piped).
        if cli.no_color || !std::io::stdout().is_terminal() {
            colored::control::set_override(false);
        }

        // verbose is handled via log-level config option

        // Use --conf-path from CLI if provided, else fall back to default
        let conf_path = cli
            .general
            .conf_path
            .as_ref()
            .and_then(|p| p.to_str())
            .map(|s| s.to_string());

        self.load_env().await;

        if let Err(e) = self.load_config_file(conf_path.as_deref()).await {
            tracing::error!("Failed to load config file: {}", e);
        }

        if let Err(e) = self.load_cli_args(cli).await {
            eprintln!("{}", format!("Argument parsing error: {}", e).red());
            return 1;
        }

        // Check daemon mode early - must happen before any output
        let daemon_mode = self.get_opt_bool("daemon").await.unwrap_or(false);
        let pid_file = self.get_opt_str("pid-file").await.map(PathBuf::from);

        if daemon_mode {
            // Check if daemon is already running
            if let Some(ref path) = pid_file {
                let pid_mgr = PidFileManager::new(path.clone());
                if let Some(existing_pid) = pid_mgr.check_existing() {
                    eprintln!(
                        "{}",
                        format!("Daemon already running with PID: {}", existing_pid).yellow()
                    );
                    return 0;
                }
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
            let console_log_level = self
                .get_opt_str("console-log-level")
                .await
                .unwrap_or_else(|| "notice".to_string());
            let log_path = self.get_opt_str("log").await;
            let log_backup_count = self.get_opt_i64("log-backup-count").await.unwrap_or(5) as usize;
            init_logging(
                &log_level,
                &console_log_level,
                log_path.as_deref(),
                log_backup_count,
            );

            info!("Daemon started successfully");
        }

        // In daemon mode, logging was already re-initialized after daemonization above.
        if !daemon_mode {
            let log_level = self
                .get_opt_str("log-level")
                .await
                .unwrap_or_else(|| "info".to_string());
            let console_log_level = self
                .get_opt_str("console-log-level")
                .await
                .unwrap_or_else(|| "notice".to_string());
            let log_path = self.get_opt_str("log").await;
            let log_backup_count = self.get_opt_i64("log-backup-count").await.unwrap_or(5) as usize;
            init_logging(
                &log_level,
                &console_log_level,
                log_path.as_deref(),
                log_backup_count,
            );
        }

        self.print_banner();

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

        // Check if there are any inputs (restored tasks or CLI URIs)
        let man = self.request_man.read().await;
        let has_restored_tasks = man.count().await > 0;

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
                    for gid in &gids {
                        println!("  {} Task #{}", "#".cyan(), gid.to_string().yellow());
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

        println!();

        // Step 6: Start RPC server (if enabled)
        let rpc_handle = if rpc_enabled {
            // Extract shared state from the engine before run() consumes it
            let (group_man, cmd_tx) = {
                let engine_lock = self.engine.lock().await;
                let engine_ref = engine_lock.as_ref().expect("engine should be initialized");
                (self.request_man.clone(), engine_ref.command_sender())
            };
            match self.start_rpc_server(group_man, cmd_tx).await {
                Ok(handle) => Some(handle),
                Err(e) => {
                    warn!("RPC server failed to start: {}", e);
                    None
                }
            }
        } else {
            None
        };

        // Step 7: Run engine
        let run_result = self.run_engine(rpc_enabled).await;

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

        match run_result {
            Ok(()) => {
                println!("{}", "All tasks completed!".green().bold());
                0
            }
            Err(e) => {
                eprintln!("{}", format!("Download failed: {}", e).red());
                1
            }
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
