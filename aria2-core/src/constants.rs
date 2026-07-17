// Protocol defaults
pub const USER_AGENT: &str = "aria2-rust/1.0";
pub const FTP_DEFAULT_PORT: u16 = 21;
pub const FTP_DEFAULT_USER: &str = "anonymous";
pub const FTP_DEFAULT_PASSWORD: &str = "aria2@";
pub const SFTP_DEFAULT_PORT: u16 = 22;

// HTTP defaults
pub const HTTP_DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 15;
pub const HTTP_DEFAULT_OVERALL_TIMEOUT_SECS: u64 = 120;
pub const HTTP_DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 300;
pub const HTTP_DEFAULT_POOL_IDLE_TIMEOUT_SECS: u64 = 90;
pub const HTTP_DEFAULT_TCP_KEEPALIVE_SECS: u64 = 60;
pub const HTTP_DEFAULT_MAX_REDIRECTS: usize = 5;
pub const HTTP_DEFAULT_POOL_MAX_IDLE_PER_HOST: usize = 8;
pub const HTTP_SPEED_UPDATE_INTERVAL_MS: u64 = 500;
pub const HTTP_DEFAULT_ERROR_CODE: u16 = 500;

// HTTP client pool defaults (higher concurrency than per-download defaults)
pub const HTTP_CLIENT_POOL_MAX_IDLE_PER_HOST: usize = 16;
pub const HTTP_CLIENT_POOL_IDLE_TIMEOUT_SECS: u64 = 300;

// HTTP connection manager config defaults
pub const HTTP_CONFIG_DEFAULT_MAX_CONNECTIONS: usize = 16;
pub const HTTP_CONFIG_DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 30;
pub const HTTP_CONFIG_DEFAULT_READ_TIMEOUT_SECS: u64 = 60;
pub const HTTP_CONFIG_DEFAULT_WRITE_TIMEOUT_SECS: u64 = 60;
pub const HTTP_CONFIG_DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;

// CORS defaults
pub const CORS_ALLOW_METHODS: &str = "POST, GET, OPTIONS";
pub const CORS_ALLOW_HEADERS: &str = "Content-Type, Authorization";
pub const CORS_MAX_AGE: &str = "86400";

// Retry defaults
pub const DEFAULT_MAX_RETRIES: u32 = 3;
pub const DEFAULT_RETRY_WAIT_SECS: u64 = 1;
pub const RETRYABLE_HTTP_CODES: [u16; 6] = [408, 429, 500, 502, 503, 504];

// BitTorrent defaults
pub const BT_BLOCK_SIZE: usize = 16384;
pub const BT_MAX_RETRIES: u32 = 3;
pub const BT_BLOCK_REQUEST_TIMEOUT_SECS: u64 = 3;
pub const BT_MAX_BLOCK_READ_MESSAGES: usize = 10000;
pub const BT_PUBLIC_TRACKER_PEER_THRESHOLD: usize = 15;
pub const BT_MAX_PUBLIC_TRACKERS_TO_TRY: usize = 10;
pub const BT_DEFAULT_MAX_UPLOAD_SLOTS: usize = 4;
pub const BT_CHOKE_ROTATION_INTERVAL_SECS: u64 = 10;
pub const BT_OPTIMISTIC_UNCHOKE_INTERVAL_SECS: u64 = 30;
pub const BT_SNUBBED_TIMEOUT_SECS: u64 = 60;
pub const BT_ENDGAME_THRESHOLD: usize = 20;
pub const BT_PEER_CONNECTION_DELAY_MS: u64 = 100;
pub const BT_MAX_UNCHOKE_WAIT_ATTEMPTS: usize = 50;
pub const BT_PEER_MESSAGE_TIMEOUT_SECS: u64 = 5;
pub const BT_HANDSHAKE_RESPONSE_SIZE: usize = 68;
pub const BT_RECEIVE_BUFFER_SIZE: usize = 4096;
pub const BT_RETRY_DELAY_MS: u64 = 100;

// Segment/Download defaults
pub const DEFAULT_FILE_ALLOCATION: &str = "falloc";
/// Default value for the `secure-falloc` option. When `false`, fallocate-based
/// allocation on platforms that don't zero-fill allocated blocks (macOS
/// `F_PREALLOCATE`, Windows `SetFileValidData`) leaves residual disk data
/// accessible until overwritten. Set to `true` to zero-fill at a performance
/// cost. Linux `fallocate(2)` always returns zeroed blocks, so this option has
/// no effect there.
pub const DEFAULT_SECURE_FALLOC: bool = false;
pub const CONCURRENT_MIN_FILE_SIZE: usize = 1024 * 1024;
pub const PROGRESS_UPDATE_BYTES: usize = 256 * 1024;
pub const DEFAULT_MAX_CONNECTION_PER_SERVER: usize = 4;
pub const DEFAULT_SPLIT: u16 = 5;
pub const MIN_SEGMENT_SIZE: usize = 1024 * 256;
pub const MAX_SEGMENT_SIZE: usize = 1024 * 1024 * 16;
pub const DEFAULT_SEGMENT_SIZE: usize = 1_048_576;
pub const DEFAULT_MAX_CONNECTIONS_PER_MIRROR: usize = 2;
pub const MAX_RETRIES_PER_SEGMENT: u32 = 3;
pub const MAX_MIRROR_FAILURES: u32 = 3;

// Mirror coordinator defaults
pub const MIRROR_SPEED_THRESHOLD: u64 = 10_000;
pub const MIRROR_COOLDOWN_SECS: u64 = 60;

// Rate limiter defaults
pub const DEFAULT_BURST_BYTES: usize = 256 * 1024;
pub const RATE_LIMITER_CHUNK_SIZE: usize = 8192;
pub const RATE_LIMITER_MIN_CHUNK_SIZE: usize = 512;
pub const RATE_LIMITER_MIN_WAIT_SECS: f64 = 0.000001;

// FTP defaults
pub const FTP_DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 300;
pub const FTP_WELCOME_TIMEOUT_SECS: u64 = 15;
pub const FTP_COMMAND_TIMEOUT_SECS: u64 = 30;
pub const FTP_TRANSFER_COMPLETE_TIMEOUT_SECS: u64 = 10;
pub const FTP_DATA_CONNECTION_TIMEOUT_SECS: u64 = 30;
pub const FTP_BUFFER_SIZE: usize = 65536;
pub const FTP_SPEED_UPDATE_INTERVAL_MS: u64 = 500;
pub const FTP_BASE_RETRY_WAIT_MS: u64 = 1000;

// FTP connection pool defaults
pub const FTP_POOL_DEFAULT_MAX_CONNECTIONS: usize = 16;
pub const FTP_POOL_DEFAULT_MAX_IDLE_TIME_SECS: u64 = 300;
pub const FTP_POOL_DEFAULT_MAX_CONNECTION_AGE_SECS: u64 = 1800;
pub const FTP_POOL_DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 30;
pub const FTP_POOL_DEFAULT_READ_TIMEOUT_SECS: u64 = 30;

// SFTP defaults
pub const SFTP_CONNECT_TIMEOUT_SECS: u64 = 15;
pub const SFTP_READ_TIMEOUT_SECS: u64 = 30;
pub const SFTP_COMMAND_TIMEOUT_SECS: u64 = 300;
pub const SFTP_DISK_WRITE_CHUNK_SIZE: usize = 64 * 1024;
pub const SFTP_SPEED_UPDATE_INTERVAL_MS: u64 = 500;

// LPD defaults
pub const LPD_MULTICAST_ADDRESS: &str = "239.192.152.143";
pub const LPD_PORT: u16 = 6771;
pub const LPD_DEFAULT_ANNOUNCE_INTERVAL_SECS: u64 = 300;
pub const LPD_RECEIVE_BUFFER_SIZE: usize = 1024;
pub const LPD_DEFAULT_RECEIVE_TIMEOUT_MS: u64 = 2000;
pub const LPD_ANNOUNCE_DISCOVER_WAIT_MS: u64 = 500;

// Choking algorithm weights
pub const CHOKING_DOWNLOAD_SPEED_WEIGHT: f64 = 0.00001;
pub const CHOKING_UPLOAD_SPEED_WEIGHT: f64 = 0.000005;
pub const CHOKING_SNUBBED_PENALTY: f64 = 1000.0;
pub const CHOKING_INTEREST_BONUS: f64 = 50.0;
pub const CHOKING_ANTI_CHURN_THRESHOLD_SECS: u64 = 60;
pub const CHOKING_ANTI_CHURN_BONUS: f64 = 30.0;

// Peer stats
pub const PEER_STATS_EMA_ALPHA: f64 = 0.5;
pub const PEER_STATS_BAD_DATA_THRESHOLD: usize = 3;

// Selector defaults
pub const DEFAULT_NB_SERVER_TO_EVALUATE: usize = 3;
pub const DEFAULT_NB_CONNECTIONS: usize = 1;

// Default paths
pub const DEFAULT_OUTPUT_DIR: &str = ".";
pub const DEFAULT_FILENAME: &str = "download";
pub const DEFAULT_HOST: &str = "localhost";
