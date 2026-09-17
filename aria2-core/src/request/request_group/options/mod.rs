mod conversion;
mod default;
mod follow_mode;
mod from_strings;
mod runtime;
#[cfg(test)]
mod tests;

pub use conversion::option_value_to_string;
pub use follow_mode::FollowMode;

pub const DEFAULT_DISK_CACHE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub split: Option<u16>,
    /// Force the HTTP downloader to use one sequential request even when the
    /// server supports ranges and `split` is greater than one.
    pub force_sequential: bool,
    pub max_connection_per_server: Option<u16>,
    pub max_download_limit: Option<u64>,
    pub max_upload_limit: Option<u64>,
    pub dir: Option<String>,
    pub out: Option<String>,
    /// Write-back disk cache capacity. Zero disables the cache.
    pub disk_cache: Option<u64>,
    /// File allocation strategy: "none", "prealloc", "falloc", "trunc", or "mmap".
    /// When "mmap", `MmapDiskWriter` is used for files above `mmap_threshold`.
    pub file_allocation: Option<String>,
    /// Allow a resumed metadata download to use a different piece length.
    pub allow_piece_length_change: bool,
    /// Enable the asynchronous DNS cache for this task.
    pub async_dns: bool,
    /// Resume an existing output file when no control file is available.
    /// This is the C++ `--continue` option and defaults to `false`.
    pub continue_download: bool,
    /// Allow replacing an existing output file. C++ default: `false`.
    pub allow_overwrite: bool,
    /// Rename an existing output file using the `.N` suffix policy.
    /// C++ default: `true`.
    pub auto_file_renaming: bool,
    /// Require a resume attempt when the remote cannot satisfy a range.
    /// C++ default: `true`.
    pub always_resume: bool,
    /// Number of failed resume attempts before a fresh download is allowed.
    /// Zero means unlimited, matching C++.
    pub max_resume_failure_tries: u32,
    /// Remove the control file before starting the download.
    pub remove_control_file: bool,
    /// File size threshold (bytes) above which mmap writes are used when
    /// `file_allocation = "mmap"`. Default: 256 MiB.
    pub mmap_threshold: Option<u64>,
    /// Enable mmap allocation when the selected allocation strategy is not
    /// `none` and the file is within `max_mmap_limit`.
    pub enable_mmap: bool,
    /// Maximum file size for mmap allocation. Zero means unlimited.
    pub max_mmap_limit: Option<u64>,
    /// Skip explicit file allocation for files smaller than this threshold.
    pub no_file_allocation_limit: Option<u64>,
    /// Zero-fill allocated blocks after fallocate on platforms that don't
    /// zero-fill (macOS `F_PREALLOCATE`, Windows `SetFileValidData`).
    /// Prevents exposure of residual disk data at a performance cost.
    /// Has no effect on Linux. Defaults to `false` (matches
    /// `constants::DEFAULT_SECURE_FALLOC`).
    pub secure_falloc: bool,
    /// Verify the existing file chunk-by-chunk against known piece hashes
    /// before downloading (C++ `--check-integrity`). Only meaningful when
    /// piece hashes are available (BitTorrent / Metalink). Defaults to `false`.
    pub check_integrity: bool,
    /// Only validate existing piece hashes; never allocate or download.
    pub hash_check_only: bool,
    /// Run the BitTorrent completion hook after a successful integrity check.
    /// The aria2 default is `true`.
    pub bt_enable_hook_after_hash_check: bool,
    /// Continue into the BitTorrent seed lifecycle after a successful
    /// integrity check of a complete payload. The aria2 default is `true`.
    pub bt_hash_check_seed: bool,
    /// Treat an existing BitTorrent payload as complete without verifying
    /// piece hashes. This is the C++ `--bt-seed-unverified` option.
    pub bt_seed_unverified: bool,
    /// Seeding time in seconds. C++ aria2 stores this as a float (minutes x 60).
    pub seed_time: Option<f64>,
    /// Seeding ratio threshold. Default: 1.0 (matches C++ PREF_SEED_RATIO default).
    pub seed_ratio: Option<f64>,
    pub checksum: Option<(String, String)>,
    pub cookie_file: Option<String>,
    pub cookies: Option<String>,
    /// Maximum active BitTorrent peer connections (C++ `BtRuntime::maxPeers_`).
    /// The tracker demand threshold is derived as 80% of this value.
    pub bt_max_peers: usize,
    /// Tracker URLs excluded from the torrent and user-supplied announce lists.
    pub bt_exclude_tracker: Option<Vec<String>>,
    /// Public IP address advertised in BitTorrent tracker announces.
    pub bt_external_ip: Option<String>,
    pub bt_force_encrypt: bool,
    pub bt_require_crypto: bool,
    /// Load a previously saved magnet metadata file before contacting peers.
    pub bt_load_saved_metadata: bool,
    /// Stop a magnet task after metadata is obtained without downloading payload.
    pub bt_metadata_only: bool,
    /// Minimum peer encryption level: `plain` or `arc4`.
    pub bt_min_crypto_level: String,
    /// Minimum peer speed used by peer admission/retention policy, in bytes/sec.
    pub bt_request_peer_speed_limit: u64,
    /// Persist magnet metadata as a `.torrent` file after exchange.
    pub bt_save_metadata: bool,
    /// Enable HTTP/FTP web-seed fallback for BitTorrent pieces.
    pub bt_enable_web_seed: bool,
    /// Maximum number of file descriptors kept open by BitTorrent writers.
    pub bt_max_open_files: usize,
    /// Optional BitTorrent peer blocklist path.
    pub bt_peer_blocklist: Option<String>,
    /// Keep-alive interval for BitTorrent peer connections, in seconds.
    pub bt_keep_alive_interval: u64,
    /// Overall BitTorrent peer inactivity timeout, in seconds.
    pub bt_timeout: u64,
    /// BitTorrent piece request timeout, in seconds.
    pub bt_request_timeout: u64,
    /// TCP connection and handshake timeout for BitTorrent peers, in seconds.
    pub peer_connection_timeout: u64,
    /// Peer ID prefix used in outgoing BitTorrent handshakes.
    pub peer_id_prefix: String,
    /// Client agent advertised in the BEP 10 extension handshake.
    pub peer_agent: String,
    /// DHT message timeout, in seconds.
    pub dht_message_timeout: u64,
    /// Enable IPv6 DHT transport.
    pub enable_dht6: bool,
    /// IPv6 DHT listen address.
    pub dht_listen_addr6: Option<String>,
    /// Explicit IPv4 DHT bootstrap hostname.
    pub dht_entry_point_host: Option<String>,
    /// Explicit IPv4 DHT bootstrap port.
    pub dht_entry_point_port: Option<u16>,
    /// Explicit IPv6 DHT bootstrap endpoint.
    pub dht_entry_point6: Option<String>,
    /// Explicit IPv6 DHT bootstrap hostname.
    pub dht_entry_point_host6: Option<String>,
    /// Explicit IPv6 DHT bootstrap port.
    pub dht_entry_point_port6: Option<u16>,
    /// IPv6 DHT routing-table persistence path.
    pub dht_file_path6: Option<String>,
    /// IPv4 DHT listen address.
    pub dht_listen_addr: Option<String>,
    pub enable_dht: bool,
    pub dht_listen_port: Option<String>,
    /// Cumulative INDEX=PATH mappings for BitTorrent file outputs.
    pub index_out: Option<String>,
    pub dht_entry_point: Option<Vec<String>>,
    /// User-specified tracker URLs that override the torrent's own
    /// announce list (C++ `--bt-tracker`). Multiple URLs are comma or
    /// newline separated.
    pub bt_tracker: Option<Vec<String>>,
    /// User-defined tracker announce interval in seconds; zero uses tracker data.
    pub bt_tracker_interval: u64,
    /// Tracker TCP connection timeout in seconds.
    pub bt_tracker_connect_timeout: u64,
    /// Tracker request timeout in seconds.
    pub bt_tracker_timeout: u64,
    /// Total timeout for stopped tracker announces during shutdown.
    pub bt_tracker_stopped_timeout: u64,
    /// Enable BEP 11 peer exchange for non-private torrents.
    pub enable_peer_exchange: bool,
    pub enable_public_trackers: bool,
    pub bt_piece_selection_strategy: String,
    pub bt_endgame_threshold: u32,
    pub max_retries: u32,
    pub retry_wait: u64,
    pub http_proxy: Option<String>,
    pub http_proxy_user: Option<String>,
    pub http_proxy_passwd: Option<String>,
    pub all_proxy: Option<String>,
    pub all_proxy_user: Option<String>,
    pub all_proxy_passwd: Option<String>,
    pub https_proxy: Option<String>,
    pub https_proxy_user: Option<String>,
    pub https_proxy_passwd: Option<String>,
    pub ftp_proxy: Option<String>,
    pub ftp_proxy_user: Option<String>,
    pub ftp_proxy_passwd: Option<String>,
    pub no_proxy: Option<String>,
    pub dht_file_path: Option<String>,

    // ------------------------------------------------------------------
    // Choking algorithm configuration (BT tit-for-tat)
    // ------------------------------------------------------------------
    /// Maximum number of peers to unchoke simultaneously during seeding.
    /// Default: 4. Set to enable the choking algorithm.
    pub bt_max_upload_slots: Option<u32>,

    /// Interval in seconds between optimistic unchokes.
    /// Default: 30.
    pub bt_optimistic_unchoke_interval: Option<u64>,

    /// Timeout in seconds after which a peer is considered snubbed (not sending data).
    /// Default: 60.
    pub bt_snubbed_timeout: Option<u64>,

    // ------------------------------------------------------------------
    // aria2-compatible file-boundary piece priority (G2)
    // ------------------------------------------------------------------
    /// Original `head[=SIZE],tail[=SIZE]` syntax. Empty means unset; the
    /// normal BitTorrent selector remains rarest-first when it is absent.
    pub bt_prioritize_piece: String,
    /// Detach completed BitTorrent seeders from the active-download budget.
    pub bt_detach_seed_only: bool,

    // ------------------------------------------------------------------
    // uTP (UDP Transport Protocol - BEP 29)
    // ------------------------------------------------------------------
    /// Enable uTP (UDP Transport Protocol) for BitTorrent connections.
    /// This implements BEP 29 and is an experimental feature not in original aria2.
    /// Default: false.
    pub enable_utp: bool,

    /// UDP port for uTP connections. 0 = auto-assign.
    /// Experimental feature not in original aria2.
    pub utp_listen_port: Option<u16>,

    // ------------------------------------------------------------------
    // HTTP headers (C++ aria2 `--header` / RPC `header` option)
    // ------------------------------------------------------------------
    /// Custom HTTP request headers as `"Name: Value"` strings.
    /// Applied to both HEAD probes and range GETs.
    pub header: Vec<String>,
    /// Override `User-Agent` header. Also injected into the `header` list by
    /// [`DownloadOptions::parsed_headers`] when set.
    pub user_agent: Option<String>,
    /// Override `Referer` header. Also injected into the `header` list by
    /// [`DownloadOptions::parsed_headers`] when set.
    pub referer: Option<String>,
    /// Keep HTTP connections alive. C++ default: `true`.
    pub enable_http_keep_alive: bool,
    /// Enable the HTTP/1.1 pipelining hint. C++ default: `false`.
    pub enable_http_pipelining: bool,
    /// Advertise gzip/deflate response support. C++ default: `false`.
    pub http_accept_gzip: bool,
    /// Add `Pragma` and `Cache-Control: no-cache` to HTTP requests.
    pub http_no_cache: bool,
    /// Use HEAD when the remote length is unknown. C++ default: `false`.
    pub use_head: bool,
    /// Omit the HTTP `Want-Digest` request header. C++ default: `false`.
    pub no_want_digest_header: bool,
    /// Verify TLS certificates for HTTPS and the FTPS extension.
    pub check_certificate: bool,
    /// Custom CA certificate bundle used by HTTPS/FTPS TLS adapters.
    pub ca_certificate: Option<String>,
    /// Client certificate used for HTTPS mutual TLS.
    /// Corresponds to aria2_original's `certificate` option.
    pub certificate: Option<String>,
    /// Private key paired with the client certificate.
    /// Corresponds to aria2_original's `private-key` option.
    pub private_key: Option<String>,
    /// Minimum TLS version accepted by HTTPS/FTPS TLS adapters.
    pub min_tls_version: Option<String>,

    // ------------------------------------------------------------------
    // Metalink options (C++ PREF_METALINK_*)
    // ------------------------------------------------------------------
    /// Preferred Metalink file version (for example, "3.0" or "4.0").
    pub metalink_version: Option<String>,
    /// Preferred Metalink file language (RFC 5646/BCP 47 language tag).
    pub metalink_language: Option<String>,
    /// Preferred Metalink file operating system identifier.
    pub metalink_os: Option<String>,
    /// Preferred download location (e.g. "JP") from metalink:resources.
    /// Maps to C++ `PREF_METALINK_LOCATION`.
    pub metalink_location: Option<String>,
    /// Preferred protocol for metalink downloads: "http", "https", "ftp", or "none".
    /// Maps to C++ `PREF_METALINK_PREFERRED_PROTOCOL`.
    pub metalink_preferred_protocol: Option<String>,
    /// Base URI used to resolve relative Metalink resources.
    pub metalink_base_uri: Option<String>,
    /// Select specific files from a metalink by segment index (e.g. "1-3,5").
    /// Maps to C++ `PREF_SELECT_FILE`.
    pub select_file: Option<String>,
    /// Remove unselected BitTorrent files after a successful download.
    /// Maps to C++ `PREF_BT_REMOVE_UNSELECTED_FILE`.
    pub bt_remove_unselected_file: bool,
    /// Piece length in bytes for metalink downloads. Default: 1 MiB (1_048_576).
    /// Maps to C++ `PREF_PIECE_LENGTH`.
    pub piece_length: Option<u64>,
    /// Whether to use only the unique protocol per host when selecting mirrors
    /// from a metalink file. Default: `true`.
    /// Maps to C++ `PREF_METALINK_ENABLE_UNIQUE_PROTOCOL`.
    pub metalink_enable_unique_protocol: bool,
    /// Minimum range size used by segment and piece selection.
    pub min_split_size: Option<u64>,
    /// Whether parameterized URI expansion is enabled for this task.
    pub parameterized_uri: bool,
    /// Whether a spent URI may be reused after a failed mirror attempt.
    pub reuse_uri: bool,
    /// URI selection strategy: `feedback`, `inorder`, or `adaptive`.
    pub uri_selector: String,
    /// Stream piece selection strategy: `default`, `inorder`, `random`, or `geom`.
    pub stream_piece_selector: String,

    // ------------------------------------------------------------------
    // FTP options (C++ PREF_* for FTP connections)
    // ------------------------------------------------------------------
    /// I/O inactivity timeout in seconds. Default: 0 (no limit).
    /// Maps to C++ `PREF_TIMEOUT`. When set, the download is aborted after
    /// this long without receiving payload bytes.
    pub timeout: Option<u64>,
    /// TCP connection timeout in seconds. Default: 60.
    /// Maps to C++ `PREF_CONNECT_TIMEOUT`.
    pub connect_timeout: Option<u64>,
    /// Idle time in seconds before the first byte is received. Default: 10.
    /// Maps to C++ `PREF_STARTUP_IDLE_TIME`.
    pub startup_idle_time: Option<u64>,
    /// Lowest download speed limit in bytes/sec. Downloads slower than this
    /// for `connect_timeout` seconds are aborted. Default: 0 (no limit).
    /// Maps to C++ `PREF_LOWEST_SPEED_LIMIT`.
    pub lowest_speed_limit: Option<u64>,
    /// Use passive mode for FTP. Default: `true`.
    /// Maps to C++ `PREF_FTP_PASV`.
    pub ftp_pasv: bool,
    /// FTP transfer representation: `binary` or `ascii`.
    pub ftp_type: String,
    /// Apply the remote file's timestamp to the local file. Default: `false`.
    /// Maps to C++ `PREF_REMOTE_TIME`.
    pub remote_time: bool,
    /// Dry-run mode: only probe and report, do not actually download. Default: `false`.
    /// Maps to C++ `PREF_DRY_RUN`.
    pub dry_run: bool,
    /// Reuse existing FTP connections. Default: `true`.
    /// Maps to C++ `PREF_FTP_REUSE_CONNECTION`.
    pub ftp_reuse_connection: bool,

    // ------------------------------------------------------------------
    // Download options (C++ PREF_* for download behaviour)
    // ------------------------------------------------------------------
    /// Verify piece checksums in real time as data arrives. Default: `true`.
    /// Maps to C++ `PREF_REALTIME_CHUNK_CHECKSUM`.
    pub realtime_chunk_checksum: bool,
    /// Timeout in seconds after which a BitTorrent download with zero peer
    /// count is stopped. Default: 0 (disabled).
    /// Maps to C++ `PREF_BT_STOP_TIMEOUT`.
    pub bt_stop_timeout: Option<u64>,

    // ------------------------------------------------------------------
    // BitTorrent extended options (C++ PREF_BT_* / PREF_*)
    // ------------------------------------------------------------------
    /// Disable IPv6 for BitTorrent connections. Default: `false`.
    /// Maps to C++ `PREF_DISABLE_IPV6`.
    pub disable_ipv6: bool,
    /// Port range for incoming BitTorrent connections (e.g. "6881-6999").
    /// Maps to C++ `PREF_LISTEN_PORT`.
    pub listen_port: Option<String>,
    /// Enable Local Peer Discovery (LPD) for BitTorrent. Default: `false`.
    /// Maps to C++ `PREF_BT_ENABLE_LPD`.
    pub bt_enable_lpd: bool,
    /// Enable JSON-RPC/XML-RPC server. Default: `false`.
    /// Maps to C++ `PREF_ENABLE_RPC`.
    pub enable_rpc: bool,
    /// Start downloads in a paused state. Default: `false`.
    /// Maps to C++ `PREF_PAUSE`.
    pub pause: bool,
    /// Pause children created by metadata post-processing.
    pub pause_metadata: bool,
    /// Whether stopped results are retained in the session when complete.
    pub force_save: bool,
    /// Whether not-found stopped results are retained in the session.
    pub save_not_found: bool,
    /// Whether uploaded RPC metadata should be persisted to a file.
    pub rpc_save_upload_metadata: bool,
    /// Whether Content-Disposition filenames use UTF-8 by default.
    pub content_disposition_default_utf8: bool,
    /// HTTP/FTP proxy method: `get` or `tunnel`.
    pub proxy_method: String,
    /// Maximum number of not-found responses allowed for this task.
    pub max_file_not_found: u32,

    // ------------------------------------------------------------------
    // Follow options (C++ PREF_FOLLOW_TORRENT / PREF_FOLLOW_METALINK)
    // ------------------------------------------------------------------
    /// Whether to follow torrent downloads by creating child request groups
    /// when a .torrent file is downloaded. Default: `true`.
    /// Maps to C++ `PREF_FOLLOW_TORRENT` (true = follow, false = just save,
    /// "mem" = in-memory-only follow).
    pub follow_torrent: Option<FollowMode>,

    /// Whether to follow Metalink downloads by creating child request groups
    /// when a Metalink document is downloaded. Default: `true`.
    /// Maps to C++ `PREF_FOLLOW_METALINK` (true = follow, false = just save,
    /// "mem" = in-memory-only follow).
    pub follow_metalink: Option<FollowMode>,

    // ------------------------------------------------------------------
    // HTTP authentication options (C++ PREF_HTTP_AUTH_CHALLENGE, PREF_HTTP_USER, etc.)
    // ------------------------------------------------------------------
    /// Whether to enable HTTP authentication challenge handling.
    /// When true, 401 responses trigger BasicCred activation and retry.
    /// Maps to C++ `PREF_HTTP_AUTH_CHALLENGE`. Default: `false`.
    pub http_auth_challenge: bool,
    /// HTTP authentication username. Maps to C++ `PREF_HTTP_USER`.
    pub http_user: Option<String>,
    /// HTTP authentication password. Maps to C++ `PREF_HTTP_PASSWD`.
    pub http_passwd: Option<String>,
    /// FTP authentication username. Maps to C++ `PREF_FTP_USER`.
    pub ftp_user: Option<String>,
    /// FTP authentication password. Maps to C++ `PREF_FTP_PASSWD`.
    pub ftp_passwd: Option<String>,
    /// SSH host-key fingerprint in aria2's `hashType=digest` format.
    pub ssh_host_key_md: Option<String>,
    /// Whether to disable Netrc lookups. Maps to C++ `PREF_NO_NETRC`.
    pub no_netrc: bool,
    /// Path to the .netrc file for credential lookup.
    /// If not set, the default ~/.netrc path is used.
    pub netrc_path: Option<String>,

    // ------------------------------------------------------------------
    // Conditional GET options (C++ PREF_CONDITIONAL_GET)
    // ------------------------------------------------------------------
    /// Whether to enable HTTP conditional GET (If-Modified-Since).
    /// When true and the local file exists without a control file, sends
    /// If-Modified-Since with the file's modification time. If the server
    /// returns 304 Not Modified, the download is marked complete without
    /// transferring data. Maps to C++ `PREF_CONDITIONAL_GET`. Default: `false`.
    pub conditional_get: bool,

    // ------------------------------------------------------------------
    // Download event hooks (C++ PREF_ON_DOWNLOAD_*)
    // ------------------------------------------------------------------
    /// Shell command to execute when a download starts.
    /// C++: `PREF_ON_DOWNLOAD_START`. Arguments: GID hex, numFiles, firstFilePath.
    pub on_download_start: Option<String>,
    /// Shell command to execute when a download completes successfully.
    /// C++: `PREF_ON_DOWNLOAD_COMPLETE`. Arguments: GID hex, numFiles, firstFilePath.
    pub on_download_complete: Option<String>,
    /// Shell command to execute when a download fails with an error.
    /// C++: `PREF_ON_DOWNLOAD_ERROR`. Arguments: GID hex, numFiles, firstFilePath.
    pub on_download_error: Option<String>,
    /// Shell command to execute when a download is paused.
    /// C++: `PREF_ON_DOWNLOAD_PAUSE`. Arguments: GID hex, numFiles, firstFilePath.
    pub on_download_pause: Option<String>,
    /// Shell command to execute when a download is stopped (not complete/error).
    /// C++: `PREF_ON_DOWNLOAD_STOP`. Arguments: GID hex, numFiles, firstFilePath.
    pub on_download_stop: Option<String>,
    /// Shell command to execute when a BitTorrent download completes fully.
    /// C++: `PREF_ON_BT_DOWNLOAD_COMPLETE`. Arguments: GID hex, numFiles, firstFilePath.
    pub on_bt_download_complete: Option<String>,
}
