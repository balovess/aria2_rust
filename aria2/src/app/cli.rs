//! CLI argument definitions using clap derive API.
//!
//! This module defines the `CliArgs` struct that replaces the hand-rolled
//! parser in `cli_options.rs`. All option names and short forms mirror the
//! `OptionRegistry` in `aria2-core`, with conflict resolution:
//! - `-h` → help (aria2_original)
//! - `-v` → version (aria2_original)
//! - `-V` → check-integrity (aria2_original)
//! - `-L` → listen-port (additional non-conflicting alias)
//! - `--save-cookies` has no short form (matching aria2_original)
//!
//! # Boolean option semantics (`--opt[=true|false]`)
//!
//! Upstream aria2 registers every boolean option through `BooleanOptionHandler`
//! with `OptionHandler::OPT_ARG`, which `OptionParser` maps onto `getopt_long`'s
//! `optional_argument`. That yields exactly four accepted spellings:
//!
//! | Spelling         | Result                                                |
//! |------------------|-------------------------------------------------------|
//! | `--opt`          | `true` (value omitted → `A2_V_TRUE`)                   |
//! | `--opt=true`     | `true`                                                 |
//! | `--opt=false`    | `false`                                                |
//! | `--opt=<other>`  | error: "must be either 'true' or 'false'."             |
//!
//! Critically, `--opt true` (space separated) is **not** consumed as a value:
//! `optional_argument` only recognises the `=` form, so `true` falls through to
//! the positional URI list. `aria2c --continue http://host/f.bin` therefore
//! still downloads `http://host/f.bin`.
//!
//! The clap equivalent is:
//!
//! ```ignore
//! #[arg(
//!     long = "continue",
//!     num_args(0..=1),
//!     require_equals = true,
//!     default_missing_value = "true",
//!     value_name = "true|false"
//! )]
//! pub continue_dl: Option<bool>,
//! ```
//!
//! * `num_args(0..=1)` makes the value optional.
//! * `require_equals = true` reproduces `optional_argument`: the value must be
//!   attached with `=`, so clap never swallows the following whitespace
//!   separated argument.
//! * `default_missing_value = "true"` supplies the implicit `true`.
//! * clap's built-in `bool` value parser accepts only the literals `true` and
//!   `false`, matching `BooleanOptionHandler::parseArg`.
//!
//! Every boolean is `Option<bool>` rather than `bool` so that the merge step in
//! [`super::config`] can distinguish three states:
//!
//! * `None` — the user did not mention the option; keep the config-file,
//!   environment, or registry-default value.
//! * `Some(true)` — explicitly enabled on the command line.
//! * `Some(false)` — explicitly disabled on the command line; this must override
//!   a `continue=true` line in `aria2.conf`.
//!
//! A plain `bool` collapses the first and last case, which silently dropped
//! `--continue=false` style overrides.

use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand};
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
    disable_help_flag = true,
    disable_version_flag = true,
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

    /// Original aria2 version action (`-v`, `--version`).
    #[arg(short = 'v', long = "version", action = ArgAction::Version)]
    pub version: Option<bool>,

    /// Original aria2 help action (`-h`, `--help`).
    #[arg(short = 'h', long = "help", action = ArgAction::Help)]
    pub help: Option<bool>,

    /// Verbose output
    #[arg(
        long = "verbose",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub verbose: Option<bool>,

    /// Disable colored output
    #[arg(
        long = "no-color",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub no_color: Option<bool>,

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
    #[arg(long = "summary-interval")]
    pub summary_interval: Option<u64>,

    /// Configuration file path
    #[arg(long = "conf-path")]
    pub conf_path: Option<PathBuf>,

    /// Disable loading configuration file
    #[arg(
        long = "no-conf",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub no_conf: Option<bool>,

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
    #[arg(
        long = "enable-color",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_color: Option<bool>,

    /// Quiet mode
    #[arg(
        short = 'q',
        long,
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub quiet: Option<bool>,

    /// Dry run (check only, no download)
    #[arg(
        long = "dry-run",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub dry_run: Option<bool>,

    /// Run as a background daemon (detached process)
    #[arg(
        short = 'D',
        long,
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub daemon: Option<bool>,

    /// Path to PID file for daemon process management
    #[arg(long = "pid-file")]
    pub pid_file: Option<PathBuf>,

    /// Allow piece length change during download
    #[arg(
        long = "allow-piece-length-change",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub allow_piece_length_change: Option<bool>,

    /// Always resume download from available session data
    #[arg(
        long = "always-resume",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub always_resume: Option<bool>,

    /// Check file integrity by validating hash
    #[arg(
        short = 'V',
        long = "check-integrity",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub check_integrity: Option<bool>,

    /// Only download if newer than local file (HTTP conditional GET)
    #[arg(
        long = "conditional-get",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub conditional_get: Option<bool>,

    /// Read URIs from input file on-demand rather than at startup
    #[arg(
        long = "deferred-input",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub deferred_input: Option<bool>,

    /// Disable IPv6 support entirely
    #[arg(
        long = "disable-ipv6",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub disable_ipv6: Option<bool>,

    /// Only check hash integrity, do not download
    #[arg(
        long = "hash-check-only",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub hash_check_only: Option<bool>,

    /// Auto-handle Metalink documents (true/false/mem)
    #[arg(long = "follow-metalink")]
    pub follow_metalink: Option<String>,

    /// Preferred Metalink file version
    #[arg(long = "metalink-version")]
    pub metalink_version: Option<String>,

    /// Preferred Metalink file language
    #[arg(long = "metalink-language")]
    pub metalink_language: Option<String>,

    /// Preferred Metalink file operating system
    #[arg(long = "metalink-os")]
    pub metalink_os: Option<String>,

    /// Preferred Metalink server location(s)
    #[arg(long = "metalink-location")]
    pub metalink_location: Option<String>,

    /// Preferred Metalink download protocol
    #[arg(long = "metalink-preferred-protocol")]
    pub metalink_preferred_protocol: Option<String>,

    /// Enable parameterized URI support (e.g. {a,b})
    #[arg(
        short = 'P',
        long = "parameterized-uri",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub parameterized_uri: Option<bool>,

    /// Start downloads in paused state
    #[arg(
        long = "pause",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub pause: Option<bool>,

    /// Remove control file before download
    #[arg(
        long = "remove-control-file",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub remove_control_file: Option<bool>,

    /// Reuse previously used URIs if connection fails
    #[arg(
        long = "reuse-uri",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub reuse_uri: Option<bool>,

    /// Save URIs that returned 404 as not found
    #[arg(
        long = "save-not-found",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub save_not_found: Option<bool>,

    /// Force sequential download of files
    #[arg(
        short = 'Z',
        long = "force-sequential",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub force_sequential: Option<bool>,

    /// Disable netrc file parsing for authentication
    #[arg(
        short = 'n',
        long = "no-netrc",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub no_netrc: Option<bool>,

    /// Verify checksum for each chunk in real-time
    #[arg(
        long = "realtime-chunk-checksum",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub realtime_chunk_checksum: Option<bool>,

    /// Download result output format (default/full/hide)
    #[arg(long = "download-result")]
    pub download_result: Option<String>,

    /// Display file sizes in human-readable format
    #[arg(
        long = "human-readable",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub human_readable: Option<bool>,

    /// Keep result of unfinished downloads in results list
    #[arg(
        long = "keep-unfinished-download-result",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub keep_unfinished_download_result: Option<bool>,

    /// Truncate console readout to fit terminal width
    #[arg(
        long = "truncate-console-readout",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub truncate_console_readout: Option<bool>,

    /// Output all console messages to stderr instead of stdout
    #[arg(
        long = "stderr",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub stderr: Option<bool>,

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
    #[arg(long = "all-proxy")]
    pub all_proxy: Option<String>,

    /// HTTP proxy URL
    #[arg(long = "http-proxy")]
    pub http_proxy: Option<String>,

    /// HTTPS proxy URL
    #[arg(long = "https-proxy")]
    pub https_proxy: Option<String>,

    /// FTP proxy URL
    #[arg(long = "ftp-proxy")]
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
    #[arg(long = "no-proxy")]
    pub no_proxy: Option<String>,

    /// User-Agent header
    #[arg(short = 'U', long = "user-agent")]
    pub user_agent: Option<String>,

    /// Referer header
    #[arg(long)]
    pub referer: Option<String>,

    /// Custom headers (Header:Value pairs, can be repeated)
    #[arg(long)]
    pub header: Vec<String>,

    /// Cookie file to load
    #[arg(long = "load-cookies")]
    pub load_cookies: Option<PathBuf>,

    /// Cookie file to save
    #[arg(long = "save-cookies")]
    pub save_cookies: Option<PathBuf>,

    /// Connect timeout in seconds
    #[arg(long = "connect-timeout")]
    pub connect_timeout: Option<u64>,

    /// I/O timeout in seconds
    #[arg(short = 't', long)]
    pub timeout: Option<u64>,

    /// Max retry attempts
    #[arg(short = 'm', long = "max-tries")]
    pub max_tries: Option<u64>,

    /// Retry wait time in seconds
    #[arg(long = "retry-wait")]
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
    #[arg(
        long = "check-certificate",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub check_certificate: Option<bool>,

    /// Disable SSL certificate verification
    #[arg(
        long = "no-check-certificate",
        hide = true,
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub no_check_certificate: Option<bool>,

    /// CA certificate file
    #[arg(long = "ca-certificate")]
    pub ca_certificate: Option<PathBuf>,

    /// Allow overwriting existing files
    #[arg(
        long = "allow-overwrite",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub allow_overwrite: Option<bool>,

    /// Auto rename conflicting files
    #[arg(
        long = "auto-file-renaming",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub auto_file_renaming: Option<bool>,

    /// Resume partial downloads
    #[arg(
        short = 'c',
        long = "continue",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub continue_dl: Option<bool>,

    /// Disable resume of partial downloads
    #[arg(
        long = "no-continue",
        hide = true,
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub no_continue: Option<bool>,

    /// Use remote file timestamp
    #[arg(
        short = 'R',
        long = "remote-time",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub remote_time: Option<bool>,

    /// Enable HTTP persistent connection (keep-alive)
    #[arg(
        long = "enable-http-keep-alive",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_http_keep_alive: Option<bool>,

    /// Enable HTTP/1.1 pipelining
    #[arg(
        long = "enable-http-pipelining",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_http_pipelining: Option<bool>,

    /// Accept gzip-encoded HTTP responses
    #[arg(
        long = "http-accept-gzip",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub http_accept_gzip: Option<bool>,

    /// Send HTTP authentication header only after challenge
    #[arg(
        long = "http-auth-challenge",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub http_auth_challenge: Option<bool>,

    /// Send Cache-Control: no-cache with requests
    #[arg(
        long = "http-no-cache",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub http_no_cache: Option<bool>,

    /// Treat Content-Disposition filename as UTF-8
    #[arg(
        long = "content-disposition-default-utf8",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub content_disposition_default_utf8: Option<bool>,

    /// Use HEAD method for file existence checks
    #[arg(
        long = "use-head",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub use_head: Option<bool>,

    /// Omit Want-Digest header from HTTP requests
    #[arg(
        long = "no-want-digest-header",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub no_want_digest_header: Option<bool>,

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
    #[arg(
        short = 'p',
        long = "ftp-pasv",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub ftp_pasv: Option<bool>,

    /// Reuse FTP data connection across downloads
    #[arg(
        long = "ftp-reuse-connection",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub ftp_reuse_connection: Option<bool>,

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
    #[arg(
        long = "bt-seed-unverified",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_seed_unverified: Option<bool>,

    /// Save metadata as .torrent file
    #[arg(
        long = "bt-save-metadata",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_save_metadata: Option<bool>,

    /// Force BT encryption
    #[arg(
        short = 'X',
        long = "bt-force-encryption",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_force_encryption: Option<bool>,

    /// Min crypto level (plain/arc4)
    #[arg(long = "bt-min-crypto-level")]
    pub bt_min_crypto_level: Option<String>,

    /// Enable Local Peer Discovery
    #[arg(
        long = "bt-enable-lpd",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_enable_lpd: Option<bool>,

    /// Enable Local Peer Discovery (alias)
    #[arg(
        long = "enable-lpd",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_lpd: Option<bool>,

    /// UDP port for Local Peer Discovery
    #[arg(long = "lpd-listen-port")]
    pub lpd_listen_port: Option<u64>,

    /// Enable web seed (HTTP/FTP seeding)
    #[arg(
        long = "bt-enable-web-seed",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_enable_web_seed: Option<bool>,

    /// Enable DHT
    #[arg(
        long = "enable-dht",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_dht: Option<bool>,

    /// Disable DHT
    #[arg(
        long = "no-enable-dht",
        hide = true,
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub no_enable_dht: Option<bool>,

    /// DHT listen port
    #[arg(long = "dht-listen-port")]
    pub dht_listen_port: Option<String>,

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
    #[arg(
        long = "enable-peer-exchange",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_peer_exchange: Option<bool>,

    /// Auto-handle .torrent (true/false/mem)
    #[arg(long = "follow-torrent")]
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
    #[arg(
        long = "enable-utp",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_utp: Option<bool>,

    /// UDP port for uTP connections. 0 = auto-assign
    #[arg(long = "utp-listen-port")]
    pub utp_listen_port: Option<u64>,

    /// Detach seed-only downloads from main session
    #[arg(
        long = "bt-detach-seed-only",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_detach_seed_only: Option<bool>,

    /// Run hook after hash check
    #[arg(
        long = "bt-enable-hook-after-hash-check",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_enable_hook_after_hash_check: Option<bool>,

    /// Comma-separated list of tracker announce URIs to exclude
    #[arg(long = "bt-exclude-tracker")]
    pub bt_exclude_tracker: Option<String>,

    /// External IP address for BitTorrent
    #[arg(long = "bt-external-ip")]
    pub bt_external_ip: Option<String>,

    /// Seed after hash check
    #[arg(
        long = "bt-hash-check-seed",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_hash_check_seed: Option<bool>,

    /// Load saved metadata from previous session
    #[arg(
        long = "bt-load-saved-metadata",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_load_saved_metadata: Option<bool>,

    /// Network interface for Local Peer Discovery
    #[arg(long = "bt-lpd-interface")]
    pub bt_lpd_interface: Option<String>,

    /// Download only torrent metadata
    #[arg(
        long = "bt-metadata-only",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_metadata_only: Option<bool>,

    /// Remove unselected files when --select-file is used
    #[arg(
        long = "bt-remove-unselected-file",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_remove_unselected_file: Option<bool>,

    /// Require BitTorrent message encryption
    #[arg(
        long = "bt-require-crypto",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_require_crypto: Option<bool>,

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
    #[arg(
        long = "enable-dht6",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_dht6: Option<bool>,

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

    /// Set output filename for a BitTorrent file index (INDEX=PATH, repeatable)
    #[arg(short = 'O', long = "index-out")]
    pub index_out: Vec<String>,
}

// =========================================================================
// RPC Options
// =========================================================================

/// JSON-RPC/XML-RPC server options.
#[derive(Args, Debug)]
pub struct RpcArgs {
    /// Enable JSON-RPC/XML-RPC server
    #[arg(
        short = 'e',
        long = "enable-rpc",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_rpc: Option<bool>,

    /// Listen on all network interfaces
    #[arg(
        long = "rpc-listen-all",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub rpc_listen_all: Option<bool>,

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
    #[arg(
        long = "rpc-secure",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub rpc_secure: Option<bool>,

    /// Path to TLS certificate file (PEM format)
    #[arg(long = "rpc-certificate")]
    pub rpc_certificate: Option<PathBuf>,

    /// Path to TLS private key file (PEM format)
    #[arg(long = "rpc-private-key")]
    pub rpc_private_key: Option<PathBuf>,

    /// Allow all origins for RPC CORS (Access-Control-Allow-Origin: *)
    #[arg(
        long = "rpc-allow-origin-all",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub rpc_allow_origin_all: Option<bool>,

    /// Max RPC request body size
    #[arg(long = "rpc-max-request-size")]
    pub rpc_max_request_size: Option<String>,

    /// Save uploaded torrent/metadata files to a directory
    #[arg(
        long = "rpc-save-upload-metadata",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub rpc_save_upload_metadata: Option<bool>,
}

// =========================================================================
// Advanced Options
// =========================================================================

/// Advanced options: bandwidth limits, disk cache, file allocation.
#[derive(Args, Debug)]
pub struct AdvancedArgs {
    /// File allocation method (none/prealloc/falloc/trunc/mmap)
    #[arg(short = 'a', long = "file-allocation")]
    pub file_allocation: Option<String>,

    /// Zero-fill allocated space after fallocate (macOS/Windows)
    #[arg(
        long = "secure-falloc",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub secure_falloc: Option<bool>,

    /// File size threshold for mmap writes (default 256M)
    #[arg(long = "mmap-threshold")]
    pub mmap_threshold: Option<String>,

    /// Max concurrent downloads
    #[arg(short = 'j', long = "max-concurrent-downloads")]
    pub max_concurrent_downloads: Option<u64>,

    /// Overall download speed limit (0=unlimited)
    #[arg(long = "max-overall-download-limit")]
    pub max_overall_download_limit: Option<String>,

    /// Per-task download limit (0=unlimited)
    #[arg(long = "max-download-limit")]
    pub max_download_limit: Option<String>,

    /// Overall upload speed limit (0=unlimited)
    #[arg(long = "max-overall-upload-limit")]
    pub max_overall_upload_limit: Option<String>,

    /// Per-task upload limit (0=unlimited)
    #[arg(short = 'u', long = "max-upload-limit")]
    pub max_upload_limit: Option<String>,

    /// BT piece length
    #[arg(long = "piece-length")]
    pub piece_length: Option<String>,

    /// Disk cache size (0=disabled)
    #[arg(long = "disk-cache")]
    pub disk_cache: Option<String>,

    /// Stop after N seconds of completion (0=never)
    #[arg(long = "stop")]
    pub stop: Option<u64>,

    /// Force save state on every change
    #[arg(
        long = "force-save",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub force_save: Option<bool>,

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
