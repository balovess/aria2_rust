/// Options that can be changed at runtime via `aria2.changeOption`.
///
/// Matches the original C++ aria2 behaviour: almost all options are
/// runtime-changeable except a short exclusion list (dry-run,
/// enable-rpc, parameterized-uri, pause, rpc-save-upload-metadata)
/// and options that are only changeable for reserved/waiting downloads
/// or globally (e.g. `dir` is startup-only for `changeOption` — it
/// only has `setChangeOptionForReserved(true)` and
/// `setChangeGlobalOption(true)` in the C++ original).
/// Keep in sync with [`RequestGroup::update_option`].
pub const RUNTIME_CHANGEABLE_OPTIONS: &[&str] = &[
    // Connection / parallelism
    "split",
    "max-connection-per-server",
    // Connection timeout
    "connect-timeout",
    // Speed limits (take effect immediately)
    "max-download-limit",
    "max-upload-limit",
    "lowest-speed-limit",
    // Retry
    "max-retries",
    "retry-wait",
    // Output paths — note: "dir" and "out" are NOT runtime-changeable
    // for `changeOption` per C++ aria2 (only setChangeOptionForReserved /
    // setChangeGlobalOption). They are excluded from this list so that
    // `changeOption` correctly rejects them with InvalidParams.
    // File allocation
    "file-allocation",
    "mmap-threshold",
    "secure-falloc",
    // Checksum & cookies
    "checksum",
    "cookie-file",
    "cookies",
    // HTTP headers
    "header",
    "user-agent",
    "referer",
    // Proxy
    "http-proxy",
    "all-proxy",
    "https-proxy",
    "ftp-proxy",
    "no-proxy",
    // FTP
    "ftp-pasv",
    "ftp-reuse-connection",
    "remote-time",
    // BitTorrent
    "bt-force-encrypt",
    "bt-require-crypto",
    "bt-max-upload-slots",
    "bt-snubbed-timeout",
    "bt-optimistic-unchoke-interval",
    "bt-endgame-threshold",
    "bt-piece-selection-strategy",
    "bt-prioritize-piece",
    "seed-time",
    "seed-ratio",
    "bt-stop-timeout",
    "disable-ipv6",
    "listen-port",
    "bt-enable-lpd",
    // DHT
    "enable-dht",
    "dht-listen-port",
    "dht-entry-point",
    "dht-file-path",
    "enable-public-trackers",
    // uTP
    "enable-utp",
    "utp-listen-port",
    // Metalink
    "metalink-location",
    "metalink-preferred-protocol",
    "select-file",
    "piece-length",
    "metalink-enable-unique-protocol",
    // Chunk checksum
    "realtime-chunk-checksum",
];

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub split: Option<u16>,
    pub max_connection_per_server: Option<u16>,
    pub max_download_limit: Option<u64>,
    pub max_upload_limit: Option<u64>,
    pub dir: Option<String>,
    pub out: Option<String>,
    /// File allocation strategy: "none", "prealloc", "falloc", "trunc", or "mmap".
    /// When "mmap", `MmapDiskWriter` is used for files above `mmap_threshold`.
    pub file_allocation: Option<String>,
    /// File size threshold (bytes) above which mmap writes are used when
    /// `file_allocation = "mmap"`. Default: 256 MiB.
    pub mmap_threshold: Option<u64>,
    /// Zero-fill allocated blocks after fallocate on platforms that don't
    /// zero-fill (macOS `F_PREALLOCATE`, Windows `SetFileValidData`).
    /// Prevents exposure of residual disk data at a performance cost.
    /// Has no effect on Linux. Defaults to `false` (matches
    /// `constants::DEFAULT_SECURE_FALLOC`).
    pub secure_falloc: bool,
    /// Seeding time in seconds. C++ aria2 stores this as a float (minutes x 60).
    pub seed_time: Option<f64>,
    /// Seeding ratio threshold. Default: 1.0 (matches C++ PREF_SEED_RATIO default).
    pub seed_ratio: Option<f64>,
    pub checksum: Option<(String, String)>,
    pub cookie_file: Option<String>,
    pub cookies: Option<String>,
    pub bt_force_encrypt: bool,
    pub bt_require_crypto: bool,
    pub enable_dht: bool,
    pub dht_listen_port: Option<u16>,
    pub dht_entry_point: Option<Vec<String>>,
    pub enable_public_trackers: bool,
    pub bt_piece_selection_strategy: String,
    pub bt_endgame_threshold: u32,
    pub max_retries: u32,
    pub retry_wait: u64,
    pub http_proxy: Option<String>,
    pub all_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub ftp_proxy: Option<String>,
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
    // Piece selection priority mode (G2)
    // ------------------------------------------------------------------
    /// Piece selection priority: "rarest" (default), "head" (sequential from start),
    /// "tail" (sequential from end).
    pub bt_prioritize_piece: String,

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

    // ------------------------------------------------------------------
    // Metalink options (C++ PREF_METALINK_*)
    // ------------------------------------------------------------------
    /// Preferred download location (e.g. "JP") from metalink:resources.
    /// Maps to C++ `PREF_METALINK_LOCATION`.
    pub metalink_location: Option<String>,
    /// Preferred protocol for metalink downloads: "http", "https", "ftp", or "none".
    /// Maps to C++ `PREF_METALINK_PREFERRED_PROTOCOL`.
    pub metalink_preferred_protocol: Option<String>,
    /// Select specific files from a metalink by segment index (e.g. "1-3,5").
    /// Maps to C++ `PREF_SELECT_FILE`.
    pub select_file: Option<String>,
    /// Piece length in bytes for metalink downloads. Default: 1 MiB (1_048_576).
    /// Maps to C++ `PREF_PIECE_LENGTH`.
    pub piece_length: Option<u64>,
    /// Whether to use only the unique protocol per host when selecting mirrors
    /// from a metalink file. Default: `true`.
    /// Maps to C++ `PREF_METALINK_ENABLE_UNIQUE_PROTOCOL`.
    pub metalink_enable_unique_protocol: bool,

    // ------------------------------------------------------------------
    // FTP options (C++ PREF_* for FTP connections)
    // ------------------------------------------------------------------
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
    /// Network interface for LPD announcements. Default: none (auto-detect).
    /// Maps to C++ `PREF_BT_LPD_INTERFACE`.
    pub bt_lpd_interface: Option<String>,
    /// Enable JSON-RPC/XML-RPC server. Default: `false`.
    /// Maps to C++ `PREF_ENABLE_RPC`.
    pub enable_rpc: bool,
    /// Start downloads in a paused state. Default: `false`.
    /// Maps to C++ `PREF_PAUSE`.
    pub pause: bool,
}

// Manual Default impl: `enable_dht` and `enable_public_trackers` default to
// `true` (matching the load path in `option_handler.rs` and `task.rs` which
// use `unwrap_or(true)`). All other fields use their type-level defaults.
impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            split: None,
            max_connection_per_server: None,
            max_download_limit: None,
            max_upload_limit: None,
            dir: None,
            out: None,
            file_allocation: None,
            mmap_threshold: None,
            secure_falloc: false,
            seed_time: None,
            seed_ratio: Some(1.0),
            checksum: None,
            cookie_file: None,
            cookies: None,
            bt_force_encrypt: false,
            bt_require_crypto: false,
            enable_dht: true,
            dht_listen_port: None,
            dht_entry_point: None,
            enable_public_trackers: true,
            bt_piece_selection_strategy: String::new(),
            bt_endgame_threshold: 0,
            max_retries: 0,
            retry_wait: 0,
            http_proxy: None,
            all_proxy: None,
            https_proxy: None,
            ftp_proxy: None,
            no_proxy: None,
            dht_file_path: None,
            bt_max_upload_slots: None,
            bt_optimistic_unchoke_interval: None,
            bt_snubbed_timeout: None,
            bt_prioritize_piece: String::new(),
            enable_utp: false,
            utp_listen_port: None,
            header: Vec::new(),
            user_agent: None,
            referer: None,
            // Metalink
            metalink_location: None,
            metalink_preferred_protocol: None,
            select_file: None,
            piece_length: None,
            metalink_enable_unique_protocol: true,
            // FTP
            connect_timeout: None,
            startup_idle_time: None,
            lowest_speed_limit: None,
            ftp_pasv: true,
            remote_time: false,
            dry_run: false,
            ftp_reuse_connection: true,
            // Download
            realtime_chunk_checksum: true,
            bt_stop_timeout: None,
            // BitTorrent extended
            disable_ipv6: false,
            listen_port: None,
            bt_enable_lpd: false,
            bt_lpd_interface: None,
            enable_rpc: false,
            pause: false,
        }
    }
}

impl DownloadOptions {
    /// Parse the raw `header` list into `(name, value)` pairs, splitting each
    /// `"Name: Value"` entry on the first `:`. When `user_agent` or `referer`
    /// are set, they are appended as `User-Agent` / `Referer` pairs (unless an
    /// entry with the same name already exists), so callers only need to handle
    /// a single header list.
    pub fn parsed_headers(&self) -> Vec<(String, String)> {
        let mut result: Vec<(String, String)> = Vec::new();
        for raw in &self.header {
            if let Some((name, value)) = raw.split_once(':') {
                let name = name.trim().to_string();
                let value = value.trim().to_string();
                if !name.is_empty() {
                    result.push((name, value));
                }
            }
        }
        // Overlay user_agent / referer if not already present (case-insensitive).
        if let Some(ref ua) = self.user_agent
            && !has_header(&result, "User-Agent")
        {
            result.push(("User-Agent".to_string(), ua.clone()));
        }
        if let Some(ref ref_) = self.referer
            && !has_header(&result, "Referer")
        {
            result.push(("Referer".to_string(), ref_.clone()));
        }
        result
    }
}

/// Case-insensitive check whether a `(name, value)` header list already contains
/// an entry with the given name.
fn has_header(headers: &[(String, String)], name: &str) -> bool {
    headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
}
