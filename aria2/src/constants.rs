// Progress bar defaults
pub const PROGRESS_BAR_WIDTH: usize = 24;
pub const PROGRESS_BAR_MIN_WIDTH: usize = 4;
pub const PROGRESS_BAR_RENDER_INTERVAL_MS: u64 = 250;
pub const PROGRESS_BAR_DURATION_OFFSET_SECS: u64 = 1;

// Unit conversion
pub const BYTES_PER_KIB: f64 = 1024.0;
pub const BYTES_PER_MIB: f64 = 1024.0 * 1024.0;
pub const BYTES_PER_GIB: f64 = 1024.0 * 1024.0 * 1024.0;
pub const SECS_PER_HOUR: u64 = 3600;
pub const SECS_PER_MINUTE: u64 = 60;

// Download status strings (uppercase - for display/UI)
pub const STATUS_ACTIVE: &str = "ACTIVE";
pub const STATUS_WAITING: &str = "WAITING";
pub const STATUS_COMPLETE: &str = "COMPLETE";
pub const STATUS_ERROR: &str = "ERROR";
pub const STATUS_SEEDING: &str = "SEEDING";
pub const STATUS_PAUSED: &str = "PAUSED";
pub const STATUS_REMOVED: &str = "REMOVED";

// Download status strings (lowercase - for internal/session protocol)
pub const STATUS_ACTIVE_LOWER: &str = "active";
pub const STATUS_WAITING_LOWER: &str = "waiting";
pub const STATUS_COMPLETE_LOWER: &str = "complete";
pub const STATUS_ERROR_LOWER: &str = "error";
pub const STATUS_RUNNING_LOWER: &str = "running";

// Display strings
pub const DOWNLOAD_SUMMARY_HEADER: &str = "=== aria2-rust Download Summary ===";
pub const LABEL_ERROR: &str = "ERROR:";
pub const LABEL_WARNING: &str = "WARNING:";
pub const LABEL_INFO: &str = "INFO:";

// Engine defaults
pub const DEFAULT_TICK_INTERVAL_MS: u64 = 100;
pub const DEFAULT_BT_ENDGAME_THRESHOLD: usize = 20;
pub const DEFAULT_MAX_RETRIES: u32 = 3;
pub const DEFAULT_RETRY_WAIT_SECS: u64 = 1;
pub const DEFAULT_MAX_UPLOAD_SLOTS: usize = 4;
pub const DEFAULT_PIECE_STRATEGY: &str = "rarest-first";
pub const DEFAULT_PIECE_PRIORITY: &str = "rarest";
pub const DEFAULT_FILE_ALLOCATION: &str = "falloc";

// Session defaults
pub const DEFAULT_SAVE_SESSION_INTERVAL_SECS: u64 = 60;
pub const MIN_SESSION_INTERVAL_SECS: u64 = 1;
pub const SESSION_RESTORE_INTERVAL_SECS: u64 = 60;

// URI detection prefixes
pub const URI_PREFIX_HTTP: &str = "http://";
pub const URI_PREFIX_HTTPS: &str = "https://";
pub const URI_PREFIX_FTP: &str = "ftp://";
pub const URI_PREFIX_FTPS: &str = "ftps://";
pub const FILE_EXT_TORRENT: &str = ".torrent";
pub const FILE_EXT_METALINK: &str = ".metalink";

// Config paths
pub const CONFIG_DIR_NAME: &str = ".aria2";
pub const CONFIG_FILE_NAME: &str = "aria2.conf";

// RPC defaults
pub const DEFAULT_RPC_PORT: usize = 6800;
pub const DEFAULT_RPC_HOST: &str = "127.0.0.1";

// GID format
pub const GID_FORMAT: &str = "{:016x}";
