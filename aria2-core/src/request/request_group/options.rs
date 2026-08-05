/// Options that can be changed at runtime via `aria2.changeOption` for
/// **active** downloads.
///
/// These are the only 7 options with `setChangeOption(true)` in C++
/// `OptionHandlerFactory.cc`. Changes take effect immediately on the
/// running download.
pub const RUNTIME_CHANGEABLE_OPTIONS: &[&str] = &[
    "force-save",
    "save-not-found",
    "max-download-limit",
    "bt-max-peers",
    "bt-remove-unselected-file",
    "bt-request-peer-speed-limit",
    "max-upload-limit",
];

/// Options that can be changed via `aria2.changeOption` for **reserved /
/// waiting** downloads (not yet active).
///
/// Extracted from C++ `OptionHandlerFactory.cc` — all options with
/// `setChangeOptionForReserved(true)`. When `changeOption` is called on
/// an active download, these options are stored as "pending" and applied
/// when the download is paused/restarted. When called on a reserved
/// download, they take effect immediately.
pub const RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS: &[&str] = &[
    // General
    "allow-overwrite",
    "allow-piece-length-change",
    "always-resume",
    "async-dns",
    "auto-file-renaming",
    "check-integrity",
    "conditional-get",
    "continue",
    "dir",
    "enable-async-dns6",
    "enable-mmap",
    "file-allocation",
    "force-save",     // also in RUNTIME_CHANGEABLE_OPTIONS
    "save-not-found", // also in RUNTIME_CHANGEABLE_OPTIONS
    "hash-check-only",
    "max-connection-per-server",
    "max-download-limit", // also in RUNTIME_CHANGEABLE_OPTIONS
    "max-mmap-limit",
    "max-resume-failure-tries",
    "min-split-size",
    "no-file-allocation-limit",
    "pause-metadata",
    "realtime-chunk-checksum",
    "remove-control-file",
    "checksum",
    "connect-timeout",
    "lowest-speed-limit",
    "max-file-not-found",
    "max-tries",
    "max-retries", // alternate wire name used in session serialization
    "no-netrc",
    "out",
    "remote-time",
    "retry-wait",
    "reuse-uri",
    "split",
    "stream-piece-selector",
    "timeout",
    "uri-selector",
    // HTTP
    "content-disposition-default-utf8",
    "enable-http-keep-alive",
    "enable-http-pipelining",
    "header",
    "http-accept-gzip",
    "http-auth-challenge",
    "http-no-cache",
    "http-passwd",
    "http-user",
    "metalink-location",
    "referer",
    "use-head",
    "no-want-digest-header",
    "user-agent",
    // FTP
    "ftp-passwd",
    "ftp-pasv",
    "ftp-reuse-connection",
    "ftp-type",
    "ftp-user",
    "ssh-host-key-md",
    // Proxy
    "http-proxy",
    "http-proxy-passwd",
    "http-proxy-user",
    "https-proxy",
    "https-proxy-passwd",
    "https-proxy-user",
    "ftp-proxy",
    "ftp-proxy-passwd",
    "ftp-proxy-user",
    "all-proxy",
    "all-proxy-passwd",
    "all-proxy-user",
    "no-proxy",
    "proxy-method",
    // Metalink
    "select-file",
    "follow-metalink",
    "metalink-enable-unique-protocol",
    "metalink-language",
    "metalink-os",
    "metalink-preferred-protocol",
    "metalink-version",
    // BitTorrent
    "bt-enable-hook-after-hash-check",
    "bt-enable-lpd",
    "bt-exclude-tracker",
    "bt-external-ip",
    "bt-force-encrypt", // note: C++ uses "bt-force-encryption"
    "bt-hash-check-seed",
    "bt-load-saved-metadata",
    "bt-max-peers", // also in RUNTIME_CHANGEABLE_OPTIONS
    "bt-metadata-only",
    "bt-min-crypto-level",
    "bt-prioritize-piece",
    "bt-detach-seed-only",
    "bt-remove-unselected-file",   // also in RUNTIME_CHANGEABLE_OPTIONS
    "bt-request-peer-speed-limit", // also in RUNTIME_CHANGEABLE_OPTIONS
    "bt-require-crypto",
    "bt-seed-unverified",
    "bt-save-metadata",
    "bt-stop-timeout",
    "bt-tracker",
    "bt-tracker-connect-timeout",
    "bt-tracker-interval",
    "bt-tracker-timeout",
    "enable-peer-exchange",
    "follow-torrent",
    "index-out",
    "max-upload-limit", // also in RUNTIME_CHANGEABLE_OPTIONS
    "seed-time",
    "seed-ratio",
];

/// Returns true if `option_name` is changeable via `aria2.changeOption`
/// for the given download state.
///
/// Matches C++ `gatherChangeableOption` / `gatherChangeableOptionForReserved`:
/// - Active downloads: only [`RUNTIME_CHANGEABLE_OPTIONS`] apply immediately;
///   [`RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS`] can be queued as "pending".
/// - Reserved/waiting downloads: [`RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS`]
///   apply immediately.
pub fn is_option_changeable(option_name: &str, is_active: bool) -> ChangeableKind {
    if RUNTIME_CHANGEABLE_OPTIONS.contains(&option_name) {
        ChangeableKind::Immediate
    } else if RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS.contains(&option_name) {
        if is_active {
            ChangeableKind::Pending
        } else {
            ChangeableKind::Immediate
        }
    } else {
        ChangeableKind::NotChangeable
    }
}

/// Classification of how an option change is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeableKind {
    /// Option takes effect immediately.
    Immediate,
    /// Option is stored as pending and applied on next pause/restart.
    Pending,
    /// Option cannot be changed at all via changeOption.
    NotChangeable,
}

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
    /// Verify the existing file chunk-by-chunk against known piece hashes
    /// before downloading (C++ `--check-integrity`). Only meaningful when
    /// piece hashes are available (BitTorrent / Metalink). Defaults to `false`.
    pub check_integrity: bool,
    /// Only validate existing piece hashes; never allocate or download.
    pub hash_check_only: bool,
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
    /// User-specified tracker URLs that override the torrent's own
    /// announce list (C++ `--bt-tracker`). Multiple URLs are comma or
    /// newline separated.
    pub bt_tracker: Option<Vec<String>>,
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
    /// Overall per-download timeout in seconds. Default: 0 (no limit).
    /// Maps to C++ `PREF_TIMEOUT`. When set, the download is aborted if
    /// it has not completed within this many seconds.
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

    // ------------------------------------------------------------------
    // Follow options (C++ PREF_FOLLOW_TORRENT / PREF_FOLLOW_METALINK)
    // ------------------------------------------------------------------
    /// Whether to follow torrent downloads by creating child request groups
    /// when a .torrent file is downloaded. Default: `true`.
    /// Maps to C++ `PREF_FOLLOW_TORRENT` (true = follow, false = just save,
    /// "mem" = in-memory-only follow).
    pub follow_torrent: Option<bool>,

    /// Whether to follow Metalink downloads by creating child request groups
    /// when a Metalink document is downloaded. Default: `true`.
    /// Maps to C++ `PREF_FOLLOW_METALINK` (true = follow, false = just save,
    /// "mem" = in-memory-only follow).
    pub follow_metalink: Option<bool>,

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
            check_integrity: false,
            hash_check_only: false,
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
            bt_tracker: None,
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
            bt_detach_seed_only: false,
            enable_utp: false,
            utp_listen_port: None,
            header: Vec::new(),
            user_agent: None,
            referer: None,
            // Metalink
            metalink_version: None,
            metalink_language: None,
            metalink_os: None,
            metalink_location: None,
            metalink_preferred_protocol: None,
            select_file: None,
            piece_length: None,
            metalink_enable_unique_protocol: true,
            // FTP
            timeout: None,
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
            // Follow options
            follow_torrent: None,
            follow_metalink: None,
            // HTTP authentication
            http_auth_challenge: false,
            http_user: None,
            http_passwd: None,
            ftp_user: None,
            ftp_passwd: None,
            no_netrc: false,
            netrc_path: None,
            // Conditional GET
            conditional_get: false,
            // Event hooks
            on_download_start: None,
            on_download_complete: None,
            on_download_error: None,
            on_download_pause: None,
            on_download_stop: None,
            on_bt_download_complete: None,
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
