//! # aria2-core
//!
//! Core library for the aria2-rust download utility.
//!
//! This crate provides the fundamental building blocks for a high-performance,
//! multi-protocol download manager:
//!
//! - **[`config`]** — Configuration system with ~95 core options, multi-source
//!   merging (defaults → env → file → CLI), `ConfigManager` runtime manager,
//!   NetRC authentication parser, and URI list file parser.
//!
//! - **[`engine`]** — Download engine with event-loop architecture (`DownloadEngine`),
//!   command queue, timer system, and tick-based scheduling.
//!
//! - **[`request`]** — Request management layer: `RequestGroupMan` (global task manager),
//!   `RequestGroup` (per-task lifecycle: Waiting → Active → Paused → Complete/Error/Removed),
//!   segment tracking, and bitfield management.
//!
//! - **[`filesystem`]** — Disk I/O abstraction: `DiskAdaptor`, `DiskWriter`,
//!   file pre-allocation strategies, write cache (LRU eviction), and checksum verification.
//!
//! - **[`ui`]** — Console UI components: `ProgressBar`, `MultiProgress` (multi-task summary),
//!   `StatusPanel`, and formatting utilities (`format_size`, `format_speed`, `format_duration`).
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use aria2_core::config::ConfigManager;
//! use aria2_core::request::request_group_man::RequestGroupMan;
//! use aria2_core::request::request_group::DownloadOptions;
//! use aria2_core::config::OptionValue;
//!
//! #[tokio::main]
//! async fn main() {
//!     let mut config = ConfigManager::new();
//!     config.set_global_option("dir", OptionValue::Str("./downloads".into())).await.unwrap();
//!     config.set_global_option("split", OptionValue::Int(4)).await.unwrap();
//!
//!     let man = RequestGroupMan::new();
//!     let opts = DownloadOptions {
//!         split: Some(4),
//!         ..Default::default()
//!     };
//!
//!     match man.add_group(vec!["http://example.com/file.zip".into()], opts).await {
//!         Ok(gid) => println!("Started: #{}", gid.value()),
//!         Err(e) => eprintln!("Error: {}", e),
//!     }
//! }
//! ```

pub mod error;
pub mod log;
pub mod colorized_stream;
pub mod engine;
pub mod request;
pub mod segment;
pub mod filesystem;
pub mod config;
pub mod util;
pub mod ui;
pub mod retry;
pub mod validation;

use tracing::Level;

/// Initialize the logging subsystem with optional file output.
///
/// Sets up `tracing-subscriber` with console output (colorized) and optionally
/// writes to a log file. The log level controls verbosity (DEBUG/INFO/WARN/ERROR).
pub fn init_logging(level: Level, log_file: Option<&str>) {
    log::init_logging(level, log_file);
}
