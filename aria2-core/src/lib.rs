//! # aria2-core
//!
//! Core library for the aria2-rust download utility — a high-performance,
//! multi-protocol download manager rewritten in Rust.
//!
//! ## Supported Protocols & Features
//!
//! | Protocol / Feature | Status | Notes |
//! |---|---|---|
//! | HTTP / HTTPS | ✅ Full | Range requests, redirects, cookies, gzip/bzip2/chunked decoding |
//! | FTP / SFTP | ✅ Full | Passive/active mode, REST resume, LIST/MLSD parsing |
//! | BitTorrent | ✅ Full | Piece picker, choke algorithm, DHT, tracker (HTTP/UDP), seeding |
//! | Metalink | ✅ Full | v3/v4 parsing, multi-source, checksum verification, signature |
//! | Auth: Basic (RFC 7617) | ✅ | Base64 credential encoding, HTTPS-only enforcement |
//! | Auth: Digest (RFC 7616) | ✅ | MD5/SHA256/SHA512 HA1→HA2→Response chain, nonce/qop/stale |
//! | LPD (BEP 14) | ✅ | UDP multicast peer discovery on 239.192.152.143:6771 |
//! | MSE (BEP 10) | ✅ | X25519 DH key exchange, RC4 encryption, plaintext fallback |
//! | Stream Filters | ✅ | Composable GZip/BZip2/Chunked decoder pipeline |
//! | Post-Download Hooks | ✅ | Move/Rename/Touch/Exec hook chain with env injection |
//! | BT Progress Persistence | ✅ | Atomic .aria2 file save/load, C++ format compatible |
//!
//! ## Module Overview
//!
//! - **[`config`]** — Configuration system with ~95 core options, multi-source
//!   merging (defaults → env → file → CLI), `ConfigManager` runtime manager,
//!   NetRC authentication parser, and URI list file parser.
//!
//! - **[`engine`]** — Download engine with event-loop architecture (`DownloadEngine`),
//!   command queue, timer system, tick-based scheduling. Includes `BtDownloadCommand`
//!   with pluggable progress/LPD/hook managers.
//!
//! - **[`request`]** — Request management layer: `RequestGroupMan` (global task manager),
//!   `RequestGroup` (per-task lifecycle: Waiting → Active → Paused → Complete/Error/Removed),
//!   segment tracking, and bitfield management.
//!
//! - **[`auth`]** — HTTP authentication: [`BasicAuthProvider`](auth::basic_auth::BasicAuthProvider),
//!   [`DigestAuthProvider`](auth::digest_auth::DigestAuthProvider), thread-safe
//!   [`CredentialStore`](auth::credential_store::CredentialStore) with automatic secret zeroing.
//!
//! - **[`http`]** — HTTP client with connection pooling, redirect following (iterative with loop detection),
//!   stream filters ([`GzDecoder`](http::stream_filter::GzDecoder), [`ChunkedDecoder`](http::stream_filter::ChunkedDecoder)),
//!   cookie jar, and auth header builders.
//!
//! - **[`ftp`]** — FTP/SFTP protocol handler with passive/active modes, fast-path LIST parser,
//!   REST resume support, and control file management.
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

pub mod auth;
pub mod checksum;
pub mod colorized_stream;
pub mod config;
pub mod engine;
pub mod error;
pub mod filesystem;
pub mod ftp;
pub mod http;
pub mod log;
pub mod option;
pub mod rate_limiter;
pub mod request;
pub mod retry;
pub mod segment;
pub mod selector;
pub mod session;
pub mod ui;
pub mod util;
pub mod validation;

#[cfg(test)]
mod integration_tests_j2_j5;

use tracing::Level;

/// Initialize the logging subsystem with optional file output.
///
/// Sets up `tracing-subscriber` with console output (colorized) and optionally
/// writes to a log file. The log level controls verbosity (DEBUG/INFO/WARN/ERROR).
pub fn init_logging(level: Level, log_file: Option<&str>) {
    log::init_logging(level, log_file);
}
