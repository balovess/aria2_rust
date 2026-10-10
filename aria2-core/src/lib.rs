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
//! | Metalink | ✅ Full | v3/v4 parsing, multi-source, checksum verification |
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
//! - **[`engine`]** — Shared event loop, task lifecycle, scheduling, and protocol
//!   command assembly. Protocol-specific execution lives under `engine::http`,
//!   `engine::ftp`, `engine::sftp`, `engine::bittorrent`, and `engine::metalink`.
//!
//! - **[`request`]** — Request management layer: `RequestGroupMan` (global task manager),
//!   `RequestGroup` (per-task lifecycle: Waiting → Active → Paused → Complete/Error/Removed),
//!   segment tracking, and bitfield management.
//!
//! - **[`auth`]** — HTTP authentication: [`BasicAuthProvider`](auth::basic_auth::BasicAuthProvider),
//!   [`DigestAuthProvider`](auth::digest_auth::DigestAuthProvider), thread-safe
//!   [`CredentialStore`](auth::credential_store::CredentialStore) with automatic secret zeroing.
//!
//! - **[`http`]** — HTTP request policy and reusable helpers for authentication,
//!   redirects, cookies, response processing, TLS identity, and stream filters
//!   ([`GzDecoder`](http::stream_filter::GzDecoder), [`ChunkedDecoder`](http::stream_filter::ChunkedDecoder)).
//!
//! - **[`ftp`]** — FTP control/data connection support, passive/active negotiation,
//!   listing parsing, proxy handling, and connection pooling. SFTP lives in its
//!   own protocol module.
//!
//! - **[`filesystem`]** — Disk I/O abstraction: `DiskAdaptor`, `DiskWriter`,
//!   file pre-allocation strategies, write cache (LRU eviction), and checksum verification.
//!
//! - **[`ui`]** — Console UI components: `ProgressBar`, `MultiProgress` (multi-task summary),
//!   `StatusPanel`, and formatting utilities (`format_size`, `format_speed`, `format_duration`).
//!
//! ## BitTorrent Library API
//!
//! BitTorrent task state and scheduling are exposed from this crate's root so
//! downstream users do not need to depend on the internal `engine` layout.
//! Use [`PiecePicker`] for task-level piece selection, [`PieceManager`] for
//! completion and hash accounting, [`PeerBitfieldTracker`] for peer
//! availability, and [`BtMessageValidator`] for validation against torrent
//! metadata.
//!
//! ```rust,no_run
//! fn bittorrent_library_example() {
//!     #[cfg(feature = "bittorrent")]
//!     {
//!     use aria2_core::{
//!         BtMessageValidator, PeerBitfieldTracker, PieceManager, PiecePicker,
//!         PieceSelectionStrategy,
//!     };
//!
//!     let mut picker = PiecePicker::new(128);
//!     picker.set_strategy(PieceSelectionStrategy::RarestFirst);
//!     let mut peers = PeerBitfieldTracker::new(128);
//!     peers.update_peer_bitfield("peer-1", &[0xff; 16]);
//!
//!     let hashes = vec![[0u8; 20]; 128];
//!     let manager = PieceManager::new(128, 262_144, 128 * 262_144, &hashes);
//!     let validator = BtMessageValidator::new(manager.num_pieces(), manager.piece_length());
//!     assert!(validator.validate_index(0).is_ok());
//!     let _ = (picker, peers);
//!     }
//! }
//! ```
//!
//! For custom seeding integrations, `aria2-core` also exposes
//! [`BtSeedManager`](engine::bittorrent::download::seed_manager::BtSeedManager),
//! [`PieceDataProvider`](engine::bittorrent::peer::upload_session::PieceDataProvider),
//! and [`BtSeedingConfig`](engine::bittorrent::peer::upload_session::BtSeedingConfig).
//! The built-in task lifecycle remains the recommended entry point when the
//! application needs RPC-visible progress, tracker/DHT discovery, persistence,
//! and download-to-seed handoff.
//!
//! The companion `aria2-protocol` crate owns reusable protocol wire formats,
//! parsers, and clients, such as Bencode, BitTorrent messages, Handshake, and
//! `Bitfield`. `aria2-core::engine` assembles them with download policy,
//! request state, and storage.
//! Applications should import the task-facing types from `aria2_core` rather
//! than from `aria2_protocol`.
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use std::sync::Arc;
//!
//! use aria2_core::request::request_group::DownloadOptions;
//! use aria2_core::request::request_group_man::RequestGroupMan;
//! use aria2_core::DownloadEngine;
//!
//! #[tokio::main]
//! async fn main() {
//!     let groups = Arc::new(RequestGroupMan::new());
//!     let mut engine = DownloadEngine::new();
//!     engine.set_request_group_man(Arc::clone(&groups));
//!     let engine = engine.start().unwrap();
//!
//!     let download = engine.downloads().add_uri(
//!         vec!["http://example.com/file.zip".into()],
//!         DownloadOptions::default(),
//!     ).unwrap();
//!     let result = download.wait().await.unwrap();
//!     println!("{}: {}", download.gid_hex(), result.status);
//!
//!     engine.shutdown_and_wait().await.unwrap();
//! }
//! ```
//!
//! Magnet downloads use the same handle and event stream. Metadata is a
//! notification rather than a `DownloadStatus` variant, so callers do not
//! need to parse messages or poll `getFiles`:
//!
//! ```rust,no_run
//! use std::sync::Arc;
//!
//! use aria2_core::request::request_group::DownloadOptions;
//! use aria2_core::request::request_group_man::RequestGroupMan;
//! use aria2_core::{DownloadEngine, DownloadNotification};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let groups = Arc::new(RequestGroupMan::new());
//!     let mut engine = DownloadEngine::new();
//!     engine.set_request_group_man(Arc::clone(&groups));
//!     let engine = engine.start()?;
//!
//!     let mut events = engine.downloads().subscribe();
//!     let mut options = DownloadOptions::default();
//!     options.dir = Some("./downloads".into());
//!     options.out = None;
//!     options.enable_dht = true;
//!
//!     let download = engine.downloads().add_uri(
//!         vec!["magnet:?xt=urn:btih:YOUR_INFO_HASH".into()],
//!         options,
//!     )?;
//!
//!     let _metadata = events.recv_metadata_for(download.gid()).await?;
//!     for file in download.get_files().unwrap_or_default() {
//!         println!("{}: {} bytes", file.path, file.length);
//!     }
//!
//!     let _result = download.wait().await?;
//!     engine.shutdown_and_wait().await?;
//!     Ok(())
//! }
//! ```

// `async_trait` marks its boxed futures as `must_use`; Clippy 1.99 also sees
// the wrapped `Result` and flags the generated trait methods as double-must-use.
#![allow(clippy::double_must_use)]

pub mod auth;
pub mod c_api;
pub mod checksum;
pub mod colorized_stream;
pub mod config;
pub mod constants;
#[cfg(feature = "bittorrent")]
pub use aria2_protocol::bittorrent::dht;
pub mod dns;
pub mod download;
pub mod engine;
pub mod error;
pub mod filesystem;
pub mod ftp;
pub mod http;
pub mod log;
pub mod network;
pub mod rate_limiter;
pub mod request;
pub mod retry;
pub mod segment;
pub mod selector;
pub mod session;
pub mod ui;
pub mod util;
pub mod validation;

// Re-export commonly used types for downstream crates.
// This avoids forcing consumers to depend on internal module paths.
#[cfg(feature = "bittorrent")]
pub use engine::bittorrent::peer::message_validation::{
    BtMessageValidationError, BtMessageValidator, MAX_BLOCK_LENGTH,
};
#[cfg(feature = "bittorrent")]
pub use engine::bittorrent::piece::{
    Bitfield, PeerBitfieldEntry, PeerBitfieldTracker, PeerTrackerStats, PickedPiece, PieceInfo,
    PieceManager, PiecePicker, PiecePickerConfig, PiecePriorityMode, PieceSelectionStrategy,
};
#[cfg(feature = "bittorrent")]
pub use engine::bittorrent::torrent::file_layout::TorrentFileEntry;
pub use engine::download_engine::DownloadEngine;
pub use engine::download_event_hooks::{
    DownloadEvent, DownloadEventHooks, DownloadEventListener, DownloadEventListenerId,
    DownloadEventStream, DownloadNotification, MetadataResolvedEvent,
};
pub use engine::download_manager::{
    DownloadEngineHandle, DownloadHandle, DownloadManager, DownloadManagerError,
};
pub use request::request_group::{
    ChangeableKind, DownloadOptions, DownloadResult, DownloadStatus, DownloadStatusSnapshot,
    FileEntry, GroupId, RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS, RUNTIME_CHANGEABLE_OPTIONS,
    UriEntry, is_option_changeable,
};
pub use request::request_group_man::{PositionMode, RequestGroupMan};

#[cfg(test)]
mod integration_tests_j2_j5;

/// Initialize the logging subsystem with optional file output.
///
/// Sets up `tracing-subscriber` with console output (colorized) and optionally
/// writes to a log file. When `log_max_size` is `Some`, size-based rotation is
/// used (keeping `log_max_files` rotated copies); otherwise daily time-based
/// rotation is used (controlled by `log_backup_count`).
#[allow(clippy::too_many_arguments)]
pub fn init_logging(
    log_level: &str,
    console_log_level: &str,
    log_file: Option<&str>,
    log_backup_count: usize,
    log_max_size: Option<u64>,
    log_max_files: Option<usize>,
) {
    log::init_logging(
        log_level,
        console_log_level,
        log_file,
        log_backup_count,
        log_max_size,
        log_max_files,
    );
}
