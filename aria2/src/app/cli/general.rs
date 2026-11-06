use std::path::PathBuf;

use clap::{ArgAction, Args};

// =========================================================================
// General Options
// =========================================================================

/// General options: directory, output, logging, UI, session management.
#[derive(Args, Debug)]
#[command(next_help_heading = "General options")]
pub struct GeneralArgs {
    /// Connect the TUI to an existing JSON-RPC endpoint instead of a local engine.
    #[arg(long = "rpc-url", value_name = "URL")]
    pub rpc_url: Option<String>,
    /// Secret token for the remote JSON-RPC endpoint.
    #[arg(long = "rpc-token", value_name = "TOKEN")]
    pub remote_rpc_secret: Option<String>,
    /// Run the interactive terminal user interface.
    #[arg(long = "tui")]
    pub tui: bool,
    /// TUI language (`en-US` or `zh-CN`; defaults to the system locale).
    #[arg(long = "language", visible_alias = "lang", value_name = "LOCALE")]
    pub language: Option<String>,
    /// Initialize a configuration and persistent-state layout, then exit.
    #[arg(long = "init", conflicts_with_all = ["show_paths", "check_config", "repair_config", "reset_config"])]
    pub init: bool,
    /// Print the resolved platform paths and exit.
    #[arg(long = "show-paths", conflicts_with_all = ["init", "check_config", "repair_config", "reset_config"])]
    pub show_paths: bool,
    /// Initialization profile: system, current, executable, portable, or custom.
    #[arg(long = "profile")]
    pub profile: Option<String>,
    /// Persistent state/configuration directory used by --init.
    #[arg(long = "state-dir")]
    pub state_dir: Option<PathBuf>,
    /// Download directory used by --init.
    #[arg(long = "download-dir")]
    pub download_dir: Option<PathBuf>,
    /// Do not prompt during --init; default profile is system.
    #[arg(long = "non-interactive")]
    pub non_interactive: bool,
    /// Compatibility flag; --init always backs up and replaces existing configuration.
    #[arg(long = "force")]
    pub force: bool,
    /// Save directory
    #[arg(short = 'd', long)]
    pub dir: Option<PathBuf>,
    /// Output filename
    #[arg(short = 'o', long)]
    pub out: Option<String>,
    /// Log file path
    #[arg(short = 'l', long)]
    pub log: Option<PathBuf>,
    /// Number of backup log files to keep
    #[arg(long = "log-backup-count")]
    pub log_backup_count: Option<u64>,
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
    /// Enable the low-frequency background update check
    #[arg(
        long = "update-check",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub update_check: Option<bool>,
    /// Minimum number of days between update checks (1-365)
    #[arg(long = "update-check-interval-days", value_name = "DAYS")]
    pub update_check_interval_days: Option<u64>,
    /// Validate configuration and exit without starting downloads
    #[arg(
        long = "check-config",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub check_config: Option<bool>,
    /// Disable invalid config entries in-place after creating a backup
    #[arg(
        long = "repair-config",
        action = ArgAction::SetTrue,
        requires = "conf_path",
        conflicts_with_all = ["check_config", "reset_config", "no_conf"]
    )]
    pub repair_config: bool,
    /// Reset a config file to built-in defaults after creating a backup
    #[arg(
        long = "reset-config",
        action = ArgAction::SetTrue,
        requires = "conf_path",
        conflicts_with_all = ["check_config", "repair_config", "no_conf"]
    )]
    pub reset_config: bool,
    /// URI input file
    #[arg(short = 'i', long = "input-file")]
    pub input_file: Option<PathBuf>,
    /// Session save file
    #[arg(long = "save-session")]
    pub save_session: Option<PathBuf>,
    /// Auto-save session interval (0=disabled)
    #[arg(long = "save-session-interval")]
    pub save_session_interval: Option<u64>,
    /// Save a control file (*.aria2) every N seconds during downloads
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
    /// Max number of downloads to start (0=unlimited)
    #[arg(long = "max-downloads")]
    pub max_downloads: Option<u64>,
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
    /// DNS resolution timeout in seconds
    #[arg(long = "dns-timeout", hide = true)]
    pub dns_timeout: Option<u64>,
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
    /// Select the least-used host for URI selection
    #[arg(
        long = "select-least-used-host",
        hide = true,
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub select_least_used_host: Option<bool>,
    /// Startup idle time in seconds
    #[arg(long = "startup-idle-time", hide = true)]
    pub startup_idle_time: Option<u64>,
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
