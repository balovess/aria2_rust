//! CLI argument definitions using clap derive API.
//!
//! This module defines the `CliArgs` struct that replaces the hand-rolled
//! parser in `cli_options.rs`. All option names and short forms mirror the
//! `OptionRegistry` in `aria2-core`, with conflict resolution:
//! - `-h` → help (aria2_original)
//! - `-v` → version (aria2_original)
//! - `-V` → check-integrity (aria2_original)
//! - `-L` → listen-port (Rust additive alias)
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

use std::ffi::OsString;
use std::path::PathBuf;
use std::str::FromStr;

use clap::{Arg, ArgAction, Args, CommandFactory, Parser, Subcommand};
use colored::Colorize;

use super::App;

/// The optional argument accepted by aria2's `-h`/`--help` option.
///
/// This is deliberately kept separate from the typed configuration option
/// parser. Help is a process-level command, not a value that is applied to a
/// download task or exposed through RPC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HelpRequest {
    /// Show the default basic help section.
    Basic,
    /// Show help selected by an aria2 tag (`#http`) or option-name keyword.
    Filter(String),
}

impl FromStr for HelpRequest {
    type Err = std::convert::Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() {
            Ok(Self::Basic)
        } else {
            Ok(Self::Filter(value.to_owned()))
        }
    }
}

// =========================================================================
// Top-level CLI struct
// =========================================================================

/// Command-line arguments for the aria2-compatible binary.
///
/// `name = "aria2c"` preserves the established executable entry point for
/// existing clients. The displayed version still comes from this product's
/// package metadata, and the binary remains `aria2c` via `[[bin]]` in
/// `aria2/Cargo.toml`.
#[derive(Parser, Debug)]
#[command(
    name = aria2_protocol::identity::PRODUCT_NAME,
    version = aria2_protocol::identity::PRODUCT_VERSION,
    disable_help_flag = true,
    disable_version_flag = true,
    disable_help_subcommand = true,
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

    /// aria2-compatible version action using the aria2-rust product version.
    #[arg(short = 'v', long = "version", action = ArgAction::Version)]
    pub version: Option<bool>,

    /// Original aria2 help action (`-h`, `--help[=TAG|KEYWORD]`).
    ///
    /// `require_equals` is important here: aria2's optional argument is only
    /// consumed in the `--help=value` form, so `--help URI` leaves `URI` as a
    /// positional input instead of treating it as a help filter.
    #[arg(
        short = 'h',
        long = "help",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "",
        value_name = "TAG|KEYWORD"
    )]
    pub help: Option<HelpRequest>,

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

impl CliArgs {
    /// Parse process arguments after preserving getopt's attached `-hVALUE`
    /// optional-argument form. Clap otherwise treats the remainder as a short
    /// option cluster (`-h` + `-V` + ...), which changes aria2's argv contract.
    pub fn parse() -> Self {
        <Self as Parser>::parse_from(normalize_short_help_args(std::env::args_os()))
    }

    /// Testable equivalent of [`Parser::try_parse_from`] with aria2 argv
    /// normalization applied before clap sees the tokens.
    pub fn try_parse_from<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let args = args.into_iter().map(Into::into).collect::<Vec<_>>();
        <Self as Parser>::try_parse_from(normalize_short_help_args(args))
    }
}

fn normalize_short_help_args<I>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    args.into_iter()
        .map(|arg| {
            let Some(value) = arg.to_str() else {
                return arg;
            };
            let suffix = value.strip_prefix("-h").unwrap_or_default();
            if suffix.starts_with('=') {
                // getopt's short optional argument includes this '='. The
                // original option_processing.cc then truncates at the first
                // '=' and falls back to the basic help section.
                OsString::from("-h")
            } else if !suffix.is_empty() {
                OsString::from(format!("-h={suffix}"))
            } else {
                arg
            }
        })
        .collect()
}

/// Render help without entering the application lifecycle.
///
/// The original executable treats help filters as an output concern. Keeping
/// that behaviour here prevents a help selector from being applied as a
/// configuration option and gives tests a pure seam for the process-level
/// command. Keyword filtering is exact on the public long option name; tag
/// filtering uses the CLI's explicit option headings and the original tag
/// names that have a direct Rust representation.
pub fn render_help(request: &HelpRequest) -> String {
    // Keep the compatibility executable name in help usage while the version
    // action itself is rendered with the independent product identity.
    let mut command = CliArgs::command().name("aria2c");

    let filter = match request {
        HelpRequest::Basic => "#basic",
        HelpRequest::Filter(raw_filter) => normalize_help_filter(raw_filter),
    };
    command = command.mut_args(|arg| {
        let visible = arg.get_long().is_some() || arg.get_short().is_some();
        if visible && matches_help_filter(&arg, filter) {
            arg
        } else {
            arg.hide(true)
        }
    });

    command.render_help().to_string()
}

fn normalize_help_filter(raw_filter: &str) -> &str {
    let filter = raw_filter.strip_prefix("--").unwrap_or(raw_filter);
    filter
        .split_once('=')
        .map_or(filter, |(keyword, _)| keyword)
}

fn matches_help_filter(arg: &Arg, filter: &str) -> bool {
    let name = arg.get_long().unwrap_or_default();
    if let Some(tag) = filter.strip_prefix('#') {
        return matches_help_tag(arg, name, tag);
    }

    name.contains(filter)
}

fn matches_help_tag(arg: &Arg, name: &str, tag: &str) -> bool {
    match tag {
        "all" => true,
        "basic" => BASIC_HELP_OPTIONS.contains(&name),
        "advanced" => arg.get_help_heading() == Some("Advanced options"),
        "http" | "https" => {
            arg.get_help_heading() == Some("HTTP/FTP options")
                && (tag == "http" || name.contains("https") || name == "check-certificate")
        }
        "ftp" => arg.get_help_heading() == Some("HTTP/FTP options") && name.contains("ftp"),
        "bittorrent" => arg.get_help_heading() == Some("BitTorrent options"),
        "metalink" => name.contains("metalink") || name == "select-file",
        "rpc" => arg.get_help_heading() == Some("RPC options"),
        "cookie" => name.contains("cookie"),
        "hook" => name.contains("hook") || name.starts_with("on-"),
        "file" => name.contains("file") || matches!(name, "dir" | "out"),
        "checksum" => name.contains("check") || name.contains("hash"),
        "experimental" => name == "enable-utp",
        "deprecated" => name == "dht-message-path",
        "help" => name == "help",
        _ => false,
    }
}

// This list follows aria2's basic help surface. Options not represented by
// the current CLI are naturally omitted from the generated output.
const BASIC_HELP_OPTIONS: &[&str] = &[
    "allow-piece-length-change",
    "always-resume",
    "bt-max-peers",
    "check-integrity",
    "continue",
    "dht-listen-addr6",
    "dht-listen-port",
    "dir",
    "enable-dht",
    "enable-dht6",
    "file-allocation",
    "ftp-passwd",
    "force-sequential",
    "ftp-pasv",
    "ftp-user",
    "help",
    "index-out",
    "input-file",
    "listen-port",
    "load-cookies",
    "log",
    "max-connection-per-server",
    "max-concurrent-downloads",
    "max-overall-upload-limit",
    "max-upload-limit",
    "max-tries",
    "min-split-size",
    "no-netrc",
    "out",
    "parameterized-uri",
    "quiet",
    "save-session",
    "save-session-interval",
    "seed-ratio",
    "seed-time",
    "show-files",
    "split",
    "timeout",
    "torrent-file",
    "metalink-file",
    "http-passwd",
    "http-user",
    "user-agent",
    "version",
];

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
#[command(next_help_heading = "General options")]
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

    /// Max number of 404 not-found attempts (0=stop immediately)
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

    /// Enable asynchronous DNS resolution
    #[arg(
        long = "async-dns",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub async_dns: Option<bool>,

    /// DNS server address for async resolver
    #[arg(long = "async-dns-server")]
    pub async_dns_server: Option<String>,

    /// Enable IPv6 async DNS resolution (deprecated)
    #[arg(
        long = "enable-async-dns6",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_async_dns6: Option<bool>,

    /// Event poll method (epoll/kqueue/port/poll/select)
    #[arg(long = "event-poll")]
    pub event_poll: Option<String>,

    /// Server performance statistics input file
    #[arg(long = "server-stat-if")]
    pub server_stat_if: Option<PathBuf>,

    /// Server performance statistics output file
    #[arg(long = "server-stat-of")]
    pub server_stat_of: Option<PathBuf>,

    /// Server stat timeout in seconds (0=unlimited)
    #[arg(long = "server-stat-timeout")]
    pub server_stat_timeout: Option<u64>,

    /// Path to the .netrc file for authentication
    #[arg(long = "netrc-path")]
    pub netrc_path: Option<PathBuf>,

    /// Show file list for BitTorrent/Metalink
    #[arg(
        short = 'S',
        long = "show-files",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub show_files: Option<bool>,

    /// Path to a .torrent file
    #[arg(short = 'T', long = "torrent-file")]
    pub torrent_file: Option<PathBuf>,

    /// Path to a Metalink file
    #[arg(short = 'M', long = "metalink-file")]
    pub metalink_file: Option<PathBuf>,

    /// Checksum for verification (hashType=digest format)
    #[arg(long = "checksum")]
    pub checksum: Option<String>,

    /// Enable mmap for file allocation
    #[arg(
        long = "enable-mmap",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_mmap: Option<bool>,

    /// Max size limit for mmap (0=unlimited)
    #[arg(long = "max-mmap-limit")]
    pub max_mmap_limit: Option<String>,

    /// Whether to use only one protocol per Metalink mirror host
    #[arg(
        long = "metalink-enable-unique-protocol",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub metalink_enable_unique_protocol: Option<bool>,

    /// Base URI used to resolve relative Metalink URLs
    #[arg(long = "metalink-base-uri")]
    pub metalink_base_uri: Option<String>,

    /// Pause downloads created from metadata
    #[arg(
        long = "pause-metadata",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub pause_metadata: Option<bool>,

    /// Command on download start
    #[arg(long = "on-download-start")]
    pub on_download_start: Option<String>,

    /// Command on download stop
    #[arg(long = "on-download-stop")]
    pub on_download_stop: Option<String>,

    /// Command on download pause
    #[arg(long = "on-download-pause")]
    pub on_download_pause: Option<String>,

    /// Command on download complete
    #[arg(long = "on-download-complete")]
    pub on_download_complete: Option<String>,

    /// Command on download error
    #[arg(long = "on-download-error")]
    pub on_download_error: Option<String>,

    /// Show the console readout
    #[arg(
        long = "show-console-readout",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub show_console_readout: Option<bool>,

    /// Set soft resource limit for open files
    #[arg(long = "rlimit-nofile")]
    pub rlimit_nofile: Option<u64>,
}

// =========================================================================
// HTTP/FTP Options
// =========================================================================

/// HTTP/FTP options: proxies, headers, timeouts, connection management.
#[derive(Args, Debug)]
#[command(next_help_heading = "HTTP/FTP options")]
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

    /// Maximum concurrent segment requests per download
    #[arg(short = 's', long)]
    pub split: Option<u64>,

    /// Min split size (e.g. 1M, 20M)
    #[arg(short = 'k', long = "min-split-size")]
    pub min_split_size: Option<String>,

    /// HTTP max connections per server; adaptive download may lower it
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

    /// Client certificate file path (PEM format)
    #[arg(long = "certificate")]
    pub certificate: Option<PathBuf>,

    /// Client private key file path (PEM format)
    #[arg(long = "private-key")]
    pub private_key: Option<PathBuf>,

    /// Minimum TLS version (TLSv1.1/TLSv1.2/TLSv1.3)
    #[arg(long = "min-tls-version")]
    pub min_tls_version: Option<String>,

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

    /// SSH host key fingerprint (hashType=digest format)
    #[arg(long = "ssh-host-key-md")]
    pub ssh_host_key_md: Option<String>,
}

// =========================================================================
// BitTorrent Options
// =========================================================================

/// BitTorrent options: seeding, DHT, PEX, peer management.
#[derive(Args, Debug)]
#[command(next_help_heading = "BitTorrent options")]
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

    /// Enable public trackers
    #[arg(
        long = "enable-public-trackers",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_public_trackers: Option<bool>,

    /// Remote public tracker list sources, comma or newline separated
    #[arg(long = "bt-tracker-source")]
    pub bt_tracker_source: Option<String>,

    /// Public tracker list refresh interval in seconds
    #[arg(long = "bt-tracker-update-interval")]
    pub bt_tracker_update_interval: Option<u64>,

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

    /// IPv6 DHT bootstrap node (hostname:port)
    #[arg(long = "dht-entry-point6")]
    pub dht_entry_point6: Option<String>,

    /// Path to IPv6 DHT routing table file
    #[arg(long = "dht-file-path6")]
    pub dht_file_path6: Option<PathBuf>,

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
#[command(next_help_heading = "RPC options")]
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
#[command(next_help_heading = "Advanced options")]
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

    /// DSCP (DiffServ) IP packet marking value (0-63)
    #[arg(long = "dscp")]
    pub dscp: Option<u64>,

    /// Socket receive buffer size (0=OS default)
    #[arg(long = "socket-recv-buffer-size")]
    pub socket_recv_buffer_size: Option<String>,

    /// Max resume failure retries before a fresh download (0=unlimited)
    #[arg(long = "max-resume-failure-tries")]
    pub max_resume_failure_tries: Option<u64>,

    /// Optimize concurrent download count based on network conditions
    #[arg(
        long = "optimize-concurrent-downloads",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub optimize_concurrent_downloads: Option<bool>,
}

// =========================================================================
// Banner display (kept here for colored output integration)
// =========================================================================

pub(crate) fn product_banner_title() -> String {
    format!(
        "{} version {}",
        aria2_protocol::identity::PRODUCT_NAME,
        aria2_protocol::identity::PRODUCT_VERSION
    )
}

impl App {
    /// Print the application banner using the product identity.
    pub(super) fn print_banner(&self) {
        println!("{}", product_banner_title().green().bold());
        println!(
            "{} {}",
            "Copyright:".blue(),
            "(C) aria2-rust contributors".white()
        );
        println!();
    }
}

#[cfg(test)]
mod banner_tests {
    use super::product_banner_title;

    #[test]
    fn banner_uses_product_identity() {
        assert_eq!(
            product_banner_title(),
            format!(
                "{} version {}",
                aria2_protocol::identity::PRODUCT_NAME,
                aria2_protocol::identity::PRODUCT_VERSION
            )
        );
    }
}
