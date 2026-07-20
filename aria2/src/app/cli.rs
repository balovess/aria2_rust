//! CLI argument definitions using clap derive API.
//!
//! This module defines the `CliArgs` struct that replaces the hand-rolled
//! parser in `cli_options.rs`. All option names and short forms mirror the
//! `OptionRegistry` in `aria2-core`, with conflict resolution:
//! - `-h` → help only (clap default)
//! - `-v` → verbose only
//! - `-V` → version (clap default)
//! - `-L` → listen-port (renamed from `-h`)
//! - `--save-cookies` has no short form (was `-V`, now reserved for version)

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use colored::Colorize;

use super::App;

// =========================================================================
// Top-level CLI struct
// =========================================================================

/// Command-line arguments for aria2-rust.
///
/// `name = "aria2"` matches the upstream `aria2c` binary's `--version` output
/// format (`aria2 VERSION`). The binary itself is still `aria2c` via `[[bin]]`
/// in `aria2/Cargo.toml`; only the clap display name is overridden so that
/// `--version` prints `aria2 0.2.1` instead of `aria2c 0.2.1`.
#[derive(Parser, Debug)]
#[command(
    name = "aria2",
    version,
    about = "aria2-rust - The ultra fast download utility",
    long_about = None
)]
pub struct CliArgs {
    /// General options
    #[command(flatten)]
    pub general: GeneralArgs,

    /// HTTP/FTP options
    #[command(flatten)]
    pub http_ftp: HttpFtpArgs,

    /// BitTorrent options
    #[command(flatten)]
    pub bittorrent: BitTorrentArgs,

    /// RPC options
    #[command(flatten)]
    pub rpc: RpcArgs,

    /// Advanced options
    #[command(flatten)]
    pub advanced: AdvancedArgs,

    /// Verbose output
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// Disable colored output
    #[arg(long = "no-color")]
    pub no_color: bool,

    /// Download URIs (HTTP/HTTPS/FTP/FTPS URLs or .torrent/.metalink file paths)
    #[arg(value_name = "URI")]
    pub uris: Vec<String>,

    /// Subcommands
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// Subcommands supported by aria2c.
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Generate shell completion scripts
    Completions {
        /// Shell type (bash, zsh, fish, elvish, powershell)
        shell: clap_complete::Shell,
    },
}

// =========================================================================
// General Options
// =========================================================================

/// General options: directory, output, logging, UI, session management.
#[derive(Args, Debug)]
pub struct GeneralArgs {
    /// Save directory
    #[arg(short = 'd', long)]
    pub dir: Option<PathBuf>,

    /// Output filename
    #[arg(short = 'o', long)]
    pub out: Option<String>,

    /// Log file path
    #[arg(short = 'l', long)]
    pub log: Option<PathBuf>,

    /// Log level (debug/info/notice/warn/error)
    #[arg(long = "log-level")]
    pub log_level: Option<String>,

    /// Console log level
    #[arg(long = "console-log-level")]
    pub console_log_level: Option<String>,

    /// Progress summary interval in seconds
    #[arg(short = 'S', long = "summary-interval")]
    pub summary_interval: Option<u64>,

    /// Configuration file path
    #[arg(long = "conf-path")]
    pub conf_path: Option<PathBuf>,

    /// Disable loading configuration file
    #[arg(long = "no-conf")]
    pub no_conf: bool,

    /// URI input file
    #[arg(short = 'i', long = "input-file")]
    pub input_file: Option<PathBuf>,

    /// Session save file
    #[arg(long = "save-session")]
    pub save_session: Option<PathBuf>,

    /// Auto-save session interval (0=disabled)
    #[arg(long = "save-session-interval")]
    pub save_session_interval: Option<u64>,

    /// Auto-save interval
    #[arg(long = "auto-save-interval")]
    pub auto_save_interval: Option<u64>,

    /// Enable colored output
    #[arg(long = "enable-color")]
    pub enable_color: bool,

    /// Quiet mode
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// Dry run (check only, no download)
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Run as a background daemon (detached process)
    #[arg(short = 'D', long)]
    pub daemon: bool,

    /// Path to PID file for daemon process management
    #[arg(long = "pid-file")]
    pub pid_file: Option<PathBuf>,

    /// Allow piece length change during download
    #[arg(long = "allow-piece-length-change")]
    pub allow_piece_length_change: bool,

    /// Always resume download from available session data
    #[arg(long = "always-resume")]
    pub always_resume: bool,

    /// Check file integrity by validating hash
    #[arg(long = "check-integrity")]
    pub check_integrity: bool,

    /// Only download if newer than local file (HTTP conditional GET)
    #[arg(long = "conditional-get")]
    pub conditional_get: bool,

    /// Read URIs from input file on-demand rather than at startup
    #[arg(long = "deferred-input")]
    pub deferred_input: bool,

    /// Disable IPv6 support entirely
    #[arg(long = "disable-ipv6")]
    pub disable_ipv6: bool,

    /// Only check hash integrity, do not download
    #[arg(long = "hash-check-only")]
    pub hash_check_only: bool,

    /// Enable parameterized URI support (e.g. {a,b})
    #[arg(long = "parameterized-uri")]
    pub parameterized_uri: bool,

    /// Start downloads in paused state
    #[arg(long = "pause")]
    pub pause: bool,

    /// Remove control file before download
    #[arg(long = "remove-control-file")]
    pub remove_control_file: bool,

    /// Reuse previously used URIs if connection fails
    #[arg(long = "reuse-uri")]
    pub reuse_uri: bool,

    /// Save URIs that returned 404 as not found
    #[arg(long = "save-not-found")]
    pub save_not_found: bool,

    /// Force sequential download of files
    #[arg(long = "force-sequential")]
    pub force_sequential: bool,

    /// Disable netrc file parsing for authentication
    #[arg(long = "no-netrc")]
    pub no_netrc: bool,

    /// Verify checksum for each chunk in real-time
    #[arg(long = "realtime-chunk-checksum")]
    pub realtime_chunk_checksum: bool,

    /// Download result output format (default/full/hide)
    #[arg(long = "download-result")]
    pub download_result: Option<String>,

    /// Display file sizes in human-readable format
    #[arg(long = "human-readable")]
    pub human_readable: bool,

    /// Keep result of unfinished downloads in results list
    #[arg(long = "keep-unfinished-download-result")]
    pub keep_unfinished_download_result: bool,

    /// Truncate console readout to fit terminal width
    #[arg(long = "truncate-console-readout")]
    pub truncate_console_readout: bool,

    /// Output all console messages to stderr instead of stdout
    #[arg(long = "stderr")]
    pub stderr: bool,

    /// Max number of download results to remember
    #[arg(long = "max-download-result")]
    pub max_download_result: Option<u64>,

    /// Lowest download speed limit (if below, aborts)
    #[arg(long = "lowest-speed-limit")]
    pub lowest_speed_limit: Option<String>,

    /// Max number of 404 not-found attempts (0=unlimited)
    #[arg(long = "max-file-not-found")]
    pub max_file_not_found: Option<u64>,

    /// File size limit below which no file allocation occurs
    #[arg(long = "no-file-allocation-limit")]
    pub no_file_allocation_limit: Option<String>,

    /// Stop aria2 when process with given PID exits (0=disabled)
    #[arg(long = "stop-with-process")]
    pub stop_with_process: Option<u64>,

    /// URI selection algorithm (feedback/inorder/adaptive)
    #[arg(long = "uri-selector")]
    pub uri_selector: Option<String>,

    /// Piece selection algorithm (default/inorder/geom/random)
    #[arg(long = "stream-piece-selector")]
    pub stream_piece_selector: Option<String>,

    /// Network interface to bind to
    #[arg(long = "interface")]
    pub interface: Option<String>,

    /// Comma-separated list of interfaces for multi-homed setups
    #[arg(long = "multiple-interface")]
    pub multiple_interface: Option<String>,

    /// Set GID for the first download
    #[arg(long = "gid")]
    pub gid: Option<String>,
}

// =========================================================================
// HTTP/FTP Options
// =========================================================================

/// HTTP/FTP options: proxies, headers, timeouts, connection management.
#[derive(Args, Debug)]
pub struct HttpFtpArgs {
    /// Global proxy URL
    #[arg(short = 'p', long = "all-proxy")]
    pub all_proxy: Option<String>,

    /// HTTP proxy URL
    #[arg(short = 'P', long = "http-proxy")]
    pub http_proxy: Option<String>,

    /// HTTPS proxy URL
    #[arg(short = 'y', long = "https-proxy")]
    pub https_proxy: Option<String>,

    /// FTP proxy URL
    #[arg(short = 'F', long = "ftp-proxy")]
    pub ftp_proxy: Option<String>,

    /// All proxy username
    #[arg(long = "all-proxy-user")]
    pub all_proxy_user: Option<String>,

    /// All proxy password
    #[arg(long = "all-proxy-passwd")]
    pub all_proxy_passwd: Option<String>,

    /// HTTP proxy username
    #[arg(long = "http-proxy-user")]
    pub http_proxy_user: Option<String>,

    /// HTTP proxy password
    #[arg(long = "http-proxy-passwd")]
    pub http_proxy_passwd: Option<String>,

    /// HTTPS proxy username
    #[arg(long = "https-proxy-user")]
    pub https_proxy_user: Option<String>,

    /// HTTPS proxy password
    #[arg(long = "https-proxy-passwd")]
    pub https_proxy_passwd: Option<String>,

    /// FTP proxy username
    #[arg(long = "ftp-proxy-user")]
    pub ftp_proxy_user: Option<String>,

    /// FTP proxy password
    #[arg(long = "ftp-proxy-passwd")]
    pub ftp_proxy_passwd: Option<String>,

    /// Proxy method (get/tunnel)
    #[arg(long = "proxy-method")]
    pub proxy_method: Option<String>,

    /// Proxy exclusion list (comma-separated domains)
    #[arg(short = 'N', long = "no-proxy")]
    pub no_proxy: Option<String>,

    /// User-Agent header
    #[arg(short = 'U', long = "user-agent")]
    pub user_agent: Option<String>,

    /// Referer header
    #[arg(short = 'R', long)]
    pub referer: Option<String>,

    /// Custom headers (Header:Value pairs, can be repeated)
    #[arg(short = 'H', long)]
    pub header: Vec<String>,

    /// Cookie file to load
    #[arg(short = 'C', long = "load-cookies")]
    pub load_cookies: Option<PathBuf>,

    /// Cookie file to save
    #[arg(long = "save-cookies")]
    pub save_cookies: Option<PathBuf>,

    /// Connect timeout in seconds
    #[arg(short = 'T', long = "connect-timeout")]
    pub connect_timeout: Option<u64>,

    /// I/O timeout in seconds
    #[arg(short = 't', long)]
    pub timeout: Option<u64>,

    /// Max retry attempts
    #[arg(short = 'm', long = "max-tries")]
    pub max_tries: Option<u64>,

    /// Retry wait time in seconds
    #[arg(short = 'w', long = "retry-wait")]
    pub retry_wait: Option<u64>,

    /// Connections per download
    #[arg(short = 's', long)]
    pub split: Option<u64>,

    /// Min split size (e.g. 1M, 20M)
    #[arg(short = 'k', long = "min-split-size")]
    pub min_split_size: Option<String>,

    /// Max connections per server
    #[arg(short = 'x', long = "max-connection-per-server")]
    pub max_connection_per_server: Option<u64>,

    /// Verify SSL certificate
    #[arg(short = 'b', long = "check-certificate")]
    pub check_certificate: bool,

    /// Disable SSL certificate verification
    #[arg(long = "no-check-certificate", hide = true)]
    pub no_check_certificate: bool,

    /// CA certificate file
    #[arg(short = 'E', long = "ca-certificate")]
    pub ca_certificate: Option<PathBuf>,

    /// Allow overwriting existing files
    #[arg(short = 'O', long = "allow-overwrite")]
    pub allow_overwrite: bool,

    /// Auto rename conflicting files
    #[arg(long = "auto-file-renaming")]
    pub auto_file_renaming: bool,

    /// Resume partial downloads
    #[arg(short = 'c', long = "continue")]
    pub continue_dl: bool,

    /// Disable resume of partial downloads
    #[arg(long = "no-continue", hide = true)]
    pub no_continue: bool,

    /// Use remote file timestamp
    #[arg(long = "remote-time")]
    pub remote_time: bool,

    /// Enable HTTP persistent connection (keep-alive)
    #[arg(long = "enable-http-keep-alive")]
    pub enable_http_keep_alive: bool,

    /// Enable HTTP/1.1 pipelining
    #[arg(long = "enable-http-pipelining")]
    pub enable_http_pipelining: bool,

    /// Accept gzip-encoded HTTP responses
    #[arg(long = "http-accept-gzip")]
    pub http_accept_gzip: bool,

    /// Send HTTP authentication header only after challenge
    #[arg(long = "http-auth-challenge")]
    pub http_auth_challenge: bool,

    /// Send Cache-Control: no-cache with requests
    #[arg(long = "http-no-cache")]
    pub http_no_cache: bool,

    /// Treat Content-Disposition filename as UTF-8
    #[arg(long = "content-disposition-default-utf8")]
    pub content_disposition_default_utf8: bool,

    /// Use HEAD method for file existence checks
    #[arg(long = "use-head")]
    pub use_head: bool,

    /// Omit Want-Digest header from HTTP requests
    #[arg(long = "no-want-digest-header")]
    pub no_want_digest_header: bool,

    /// HTTP authentication username
    #[arg(long = "http-user")]
    pub http_user: Option<String>,

    /// HTTP authentication password
    #[arg(long = "http-passwd")]
    pub http_passwd: Option<String>,

    /// FTP authentication username
    #[arg(long = "ftp-user")]
    pub ftp_user: Option<String>,

    /// FTP authentication password
    #[arg(long = "ftp-passwd")]
    pub ftp_passwd: Option<String>,

    /// Use FTP passive mode
    #[arg(long = "ftp-pasv")]
    pub ftp_pasv: bool,

    /// Reuse FTP data connection across downloads
    #[arg(long = "ftp-reuse-connection")]
    pub ftp_reuse_connection: bool,

    /// FTP transfer type (binary/ascii)
    #[arg(long = "ftp-type")]
    pub ftp_type: Option<String>,
}

// =========================================================================
// BitTorrent Options
// =========================================================================

/// BitTorrent options: seeding, DHT, PEX, peer management.
#[derive(Args, Debug)]
pub struct BitTorrentArgs {
    /// Seeding time in minutes (0=infinite)
    #[arg(short = 'G', long = "seed-time")]
    pub seed_time: Option<f64>,

    /// Share ratio threshold
    #[arg(short = 'g', long = "seed-ratio")]
    pub seed_ratio: Option<f64>,

    /// Max peers per torrent
    #[arg(short = 'B', long = "bt-max-peers")]
    pub bt_max_peers: Option<u64>,

    /// Min peer speed to stay connected
    #[arg(long = "bt-request-peer-speed-limit")]
    pub bt_request_peer_speed_limit: Option<String>,

    /// Max open files for BT
    #[arg(long = "bt-max-open-files")]
    pub bt_max_open_files: Option<u64>,

    /// Seed without verifying hash
    #[arg(long = "bt-seed-unverified")]
    pub bt_seed_unverified: bool,

    /// Save metadata as .torrent file
    #[arg(long = "bt-save-metadata")]
    pub bt_save_metadata: bool,

    /// Force BT encryption
    #[arg(short = 'X', long = "bt-force-encryption")]
    pub bt_force_encryption: bool,

    /// Min crypto level (plain/arc4)
    #[arg(long = "bt-min-crypto-level")]
    pub bt_min_crypto_level: Option<String>,

    /// Enable Local Peer Discovery
    #[arg(long = "bt-enable-lpd")]
    pub bt_enable_lpd: bool,

    /// Enable Local Peer Discovery (alias)
    #[arg(long = "enable-lpd")]
    pub enable_lpd: bool,

    /// UDP port for Local Peer Discovery
    #[arg(long = "lpd-listen-port")]
    pub lpd_listen_port: Option<u64>,

    /// Enable web seed (HTTP/FTP seeding)
    #[arg(long = "bt-enable-web-seed")]
    pub bt_enable_web_seed: bool,

    /// Enable DHT
    #[arg(long = "enable-dht")]
    pub enable_dht: bool,

    /// Disable DHT
    #[arg(long = "no-enable-dht", hide = true)]
    pub no_enable_dht: bool,

    /// DHT listen port
    #[arg(long = "dht-listen-port")]
    pub dht_listen_port: Option<u64>,

    /// DHT bootstrap nodes (host:port format, comma-separated)
    #[arg(long = "dht-entry-point")]
    pub dht_entry_point: Option<String>,

    /// Path to DHT routing table file for persistence
    #[arg(long = "dht-file-path")]
    pub dht_file_path: Option<PathBuf>,

    /// DHT message cache path (deprecated)
    #[arg(long = "dht-message-path")]
    pub dht_message_path: Option<PathBuf>,

    /// Enable PEX
    #[arg(long = "enable-peer-exchange")]
    pub enable_peer_exchange: bool,

    /// Auto-handle .torrent (true/false/mem)
    #[arg(short = 'M', long = "follow-torrent")]
    pub follow_torrent: Option<String>,

    /// Command on BT download complete
    #[arg(long = "on-bt-download-complete")]
    pub on_bt_download_complete: Option<String>,

    /// Command on BT download error
    #[arg(long = "on-bt-download-error")]
    pub on_bt_download_error: Option<String>,

    /// Listening port range (e.g. 6881-6999)
    #[arg(short = 'L', long = "listen-port")]
    pub listen_port: Option<String>,

    /// Piece selection priority mode (rarest/head/tail)
    #[arg(long = "bt-prioritize-piece")]
    pub bt_prioritize_piece: Option<String>,

    /// Enable uTP (UDP Transport Protocol, BEP 29). Experimental
    #[arg(long = "enable-utp")]
    pub enable_utp: bool,

    /// UDP port for uTP connections. 0 = auto-assign
    #[arg(long = "utp-listen-port")]
    pub utp_listen_port: Option<u64>,

    /// Detach seed-only downloads from main session
    #[arg(long = "bt-detach-seed-only")]
    pub bt_detach_seed_only: bool,

    /// Run hook after hash check
    #[arg(long = "bt-enable-hook-after-hash-check")]
    pub bt_enable_hook_after_hash_check: bool,

    /// Comma-separated list of tracker announce URIs to exclude
    #[arg(long = "bt-exclude-tracker")]
    pub bt_exclude_tracker: Option<String>,

    /// External IP address for BitTorrent
    #[arg(long = "bt-external-ip")]
    pub bt_external_ip: Option<String>,

    /// Seed after hash check
    #[arg(long = "bt-hash-check-seed")]
    pub bt_hash_check_seed: bool,

    /// Load saved metadata from previous session
    #[arg(long = "bt-load-saved-metadata")]
    pub bt_load_saved_metadata: bool,

    /// Network interface for Local Peer Discovery
    #[arg(long = "bt-lpd-interface")]
    pub bt_lpd_interface: Option<String>,

    /// Download only torrent metadata
    #[arg(long = "bt-metadata-only")]
    pub bt_metadata_only: bool,

    /// Remove unselected files when --select-file is used
    #[arg(long = "bt-remove-unselected-file")]
    pub bt_remove_unselected_file: bool,

    /// Require BitTorrent message encryption
    #[arg(long = "bt-require-crypto")]
    pub bt_require_crypto: bool,

    /// Stop BT download after N seconds without progress
    #[arg(long = "bt-stop-timeout")]
    pub bt_stop_timeout: Option<u64>,

    /// Comma-separated list of tracker announce URIs
    #[arg(long = "bt-tracker")]
    pub bt_tracker: Option<String>,

    /// Connect timeout for tracker in seconds
    #[arg(long = "bt-tracker-connect-timeout")]
    pub bt_tracker_connect_timeout: Option<u64>,

    /// Tracker announce interval in seconds
    #[arg(long = "bt-tracker-interval")]
    pub bt_tracker_interval: Option<u64>,

    /// Timeout for tracker in seconds
    #[arg(long = "bt-tracker-timeout")]
    pub bt_tracker_timeout: Option<u64>,

    /// DHT message timeout in seconds
    #[arg(long = "dht-message-timeout")]
    pub dht_message_timeout: Option<u64>,

    /// Enable IPv6 DHT
    #[arg(long = "enable-dht6")]
    pub enable_dht6: bool,

    /// IPv6 address for DHT to listen on
    #[arg(long = "dht-listen-addr6")]
    pub dht_listen_addr6: Option<String>,

    /// Peer ID prefix for BitTorrent
    #[arg(long = "peer-id-prefix")]
    pub peer_id_prefix: Option<String>,

    /// Peer agent string for BitTorrent
    #[arg(long = "peer-agent")]
    pub peer_agent: Option<String>,

    /// Comma-separated list of file indices to download (BT/Metalink, 1-indexed)
    #[arg(long = "select-file")]
    pub select_file: Option<String>,
}

// =========================================================================
// RPC Options
// =========================================================================

/// JSON-RPC/XML-RPC server options.
#[derive(Args, Debug)]
pub struct RpcArgs {
    /// Enable JSON-RPC/XML-RPC server
    #[arg(short = 'e', long = "enable-rpc")]
    pub enable_rpc: bool,

    /// Listen on all network interfaces
    #[arg(long = "rpc-listen-all")]
    pub rpc_listen_all: bool,

    /// RPC server port
    #[arg(short = 'r', long = "rpc-listen-port")]
    pub rpc_listen_port: Option<u16>,

    /// RPC server bind address
    #[arg(long = "rpc-listen-address")]
    pub rpc_listen_address: Option<String>,

    /// RPC secret token for authorization
    #[arg(short = 'I', long = "rpc-secret")]
    pub rpc_secret: Option<String>,

    /// RPC Basic Auth username
    #[arg(long = "rpc-user")]
    pub rpc_user: Option<String>,

    /// RPC Basic Auth password
    #[arg(long = "rpc-passwd")]
    pub rpc_passwd: Option<String>,

    /// CORS Allow-Origin value
    #[arg(long = "rpc-allow-origin")]
    pub rpc_allow_origin: Option<String>,

    /// CORS allowed domains for RPC (comma-separated)
    #[arg(long = "rpc-cors-domain")]
    pub rpc_cors_domain: Option<String>,

    /// Enable HTTPS for RPC server
    #[arg(long = "rpc-secure")]
    pub rpc_secure: bool,

    /// Path to TLS certificate file (PEM format)
    #[arg(long = "rpc-certificate")]
    pub rpc_certificate: Option<PathBuf>,

    /// Path to TLS private key file (PEM format)
    #[arg(long = "rpc-private-key")]
    pub rpc_private_key: Option<PathBuf>,

    /// Allow all origins for RPC CORS (Access-Control-Allow-Origin: *)
    #[arg(long = "rpc-allow-origin-all")]
    pub rpc_allow_origin_all: bool,

    /// Max RPC request body size
    #[arg(long = "rpc-max-request-size")]
    pub rpc_max_request_size: Option<String>,

    /// Save uploaded torrent/metadata files to a directory
    #[arg(long = "rpc-save-upload-metadata")]
    pub rpc_save_upload_metadata: bool,
}

// =========================================================================
// Advanced Options
// =========================================================================

/// Advanced options: bandwidth limits, disk cache, file allocation.
#[derive(Args, Debug)]
pub struct AdvancedArgs {
    /// File allocation method (none/prealloc/falloc/trunc/mmap)
    #[arg(short = 'f', long = "file-allocation")]
    pub file_allocation: Option<String>,

    /// Zero-fill allocated space after fallocate (macOS/Windows)
    #[arg(long = "secure-falloc")]
    pub secure_falloc: bool,

    /// File size threshold for mmap writes (default 256M)
    #[arg(long = "mmap-threshold")]
    pub mmap_threshold: Option<String>,

    /// Max concurrent downloads
    #[arg(short = 'j', long = "max-concurrent-downloads")]
    pub max_concurrent_downloads: Option<u64>,

    /// Overall download speed limit (0=unlimited)
    #[arg(short = 'A', long = "max-overall-download-limit")]
    pub max_overall_download_limit: Option<String>,

    /// Per-task download limit (0=unlimited)
    #[arg(short = 'Q', long = "max-download-limit")]
    pub max_download_limit: Option<String>,

    /// Overall upload speed limit (0=unlimited)
    #[arg(short = 'W', long = "max-overall-upload-limit")]
    pub max_overall_upload_limit: Option<String>,

    /// Per-task upload limit (0=unlimited)
    #[arg(short = 'K', long = "max-upload-limit")]
    pub max_upload_limit: Option<String>,

    /// BT piece length
    #[arg(short = 'Y', long = "piece-length")]
    pub piece_length: Option<String>,

    /// Disk cache size (0=disabled)
    #[arg(short = 'Z', long = "disk-cache")]
    pub disk_cache: Option<String>,

    /// Stop after N seconds of completion (0=never)
    #[arg(short = 'z', long = "stop")]
    pub stop: Option<u64>,

    /// Force save state on every change
    #[arg(long = "force-save")]
    pub force_save: bool,

    /// Path to save/load server performance statistics
    #[arg(long = "server-stat-file")]
    pub server_stat_file: Option<PathBuf>,

    /// Auto-save interval for server stats in seconds (0=disabled)
    #[arg(long = "save-server-stat-interval")]
    pub save_server_stat_interval: Option<u64>,
}

// =========================================================================
// Banner display (kept here for colored output integration)
// =========================================================================

impl App {
    /// Print the application banner with version from CARGO_PKG_VERSION.
    pub(super) fn print_banner(&self) {
        println!(
            "{}",
            format!("aria2-rust v{}", env!("CARGO_PKG_VERSION"))
                .green()
                .bold()
        );
        println!(
            "{} {}",
            "Copyright:".blue(),
            "(C) 2024-2026 aria2-rust contributors".white()
        );
        println!();
    }
}
