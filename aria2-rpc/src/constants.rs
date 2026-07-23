// RPC server defaults
pub const DEFAULT_RPC_HOST: &str = "127.0.0.1";
pub const DEFAULT_RPC_PORT: u16 = 6800;
pub const RPC_SERVER_NAME: &str = "aria2-rust";

/// Default max RPC request body size in bytes (2 MiB).
/// Matches C++ PREF_RPC_MAX_REQUEST_SIZE default.
pub const DEFAULT_RPC_MAX_REQUEST_SIZE: usize = 2 * 1024 * 1024;

// CORS defaults (CORS_ALLOW_METHODS, CORS_ALLOW_HEADERS, CORS_MAX_AGE are in aria2-core)
pub const CORS_DEFAULT_ORIGIN: &str = "*";

// WebSocket defaults
pub const WS_DEFAULT_PING_INTERVAL_SECS: u64 = 30;
pub const WS_DEFAULT_PONG_TIMEOUT_SECS: u64 = 60;

// JSON-RPC
pub const JSON_RPC_VERSION: &str = "2.0";
pub const JSON_RPC_PARSE_ERROR: i64 = -32700;
pub const JSON_RPC_INVALID_REQUEST: i64 = -32600;
pub const JSON_RPC_METHOD_NOT_FOUND: i64 = -32601;
pub const JSON_RPC_INVALID_PARAMS: i64 = -32602;
pub const JSON_RPC_INTERNAL_ERROR: i64 = -32603;
pub const JSON_RPC_UNAUTHORIZED: i64 = -32001;

// Session/GID
pub const SESSION_ID_PREFIX: &str = "session-";
pub const GID_HEX_DIGITS: usize = 16;

// RPC endpoint
pub const RPC_ENDPOINT_PATH: &str = "/jsonrpc";

// WebSocket event names (matching C++ aria2 WebSocketSessionMan exactly — 6 events)
pub const WS_EVENT_DOWNLOAD_START: &str = "aria2.onDownloadStart";
pub const WS_EVENT_DOWNLOAD_PAUSE: &str = "aria2.onDownloadPause";
pub const WS_EVENT_DOWNLOAD_STOP: &str = "aria2.onDownloadStop";
pub const WS_EVENT_DOWNLOAD_COMPLETE: &str = "aria2.onDownloadComplete";
pub const WS_EVENT_DOWNLOAD_ERROR: &str = "aria2.onDownloadError";
pub const WS_EVENT_BT_DOWNLOAD_COMPLETE: &str = "aria2.onBtDownloadComplete";
