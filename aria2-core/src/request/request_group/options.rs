use crate::config::OptionValue;

/// How a metadata file should be handled after it is downloaded.
///
/// `None` on [`DownloadOptions`] means that the option was not explicitly
/// supplied and the aria2 default applies.  When supplied, the wire values
/// are `true`, `false`, and `mem`; keeping `mem` as a distinct enum variant is
/// necessary because it changes the disk-writer lifecycle, not just handler
/// selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowMode {
    /// Follow the downloaded metadata and keep the source file on disk.
    Follow,
    /// Do not follow the downloaded metadata.
    Disabled,
    /// Follow metadata from an in-memory buffer without creating a source file.
    Memory,
}

impl FollowMode {
    /// Convert the boolean form accepted by legacy RPC callers.
    pub const fn from_bool(value: bool) -> Self {
        if value { Self::Follow } else { Self::Disabled }
    }

    /// Parse an aria2 option value. Invalid values are rejected so callers
    /// can preserve the configured default instead of silently changing mode.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Some(Self::Follow),
            "false" | "0" => Some(Self::Disabled),
            "mem" => Some(Self::Memory),
            _ => None,
        }
    }

    /// Return the canonical aria2 wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Follow => "true",
            Self::Disabled => "false",
            Self::Memory => "mem",
        }
    }

    /// Whether the metadata post-download handler should be installed.
    pub const fn follows(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// Whether the source should be downloaded into memory.
    pub const fn is_memory(self) -> bool {
        matches!(self, Self::Memory)
    }
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
    /// Maximum active BitTorrent peer connections (C++ `BtRuntime::maxPeers_`).
    /// The tracker demand threshold is derived as 80% of this value.
    pub bt_max_peers: usize,
    pub bt_force_encrypt: bool,
    pub bt_require_crypto: bool,
    pub enable_dht: bool,
    pub dht_listen_port: Option<String>,
    /// Cumulative INDEX=PATH mappings for BitTorrent file outputs.
    pub index_out: Option<String>,
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
            continue_download: false,
            allow_overwrite: false,
            auto_file_renaming: true,
            always_resume: true,
            max_resume_failure_tries: 0,
            remove_control_file: false,
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
            index_out: None,
            dht_entry_point: None,
            bt_tracker: None,
            enable_public_trackers: true,
            bt_piece_selection_strategy: String::new(),
            bt_endgame_threshold: 0,
            max_retries: crate::constants::DEFAULT_MAX_RETRIES,
            retry_wait: 0,
            http_proxy: None,
            http_proxy_user: None,
            http_proxy_passwd: None,
            all_proxy: None,
            all_proxy_user: None,
            all_proxy_passwd: None,
            https_proxy: None,
            https_proxy_user: None,
            https_proxy_passwd: None,
            ftp_proxy: None,
            ftp_proxy_user: None,
            ftp_proxy_passwd: None,
            no_proxy: None,
            dht_file_path: None,
            bt_max_peers: 55,
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
            enable_http_keep_alive: true,
            enable_http_pipelining: false,
            http_accept_gzip: false,
            http_no_cache: false,
            use_head: false,
            no_want_digest_header: false,
            check_certificate: true,
            ca_certificate: None,
            min_tls_version: None,
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
            ssh_host_key_md: None,
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

/// Convert a JSON-RPC option value to the string representation used by
/// aria2's option handlers.
///
/// JSON-RPC/XML-RPC callers normally provide strings. Numeric and boolean
/// values are accepted by the Rust API as an extension, while arrays are
/// joined for cumulative options such as `header`.
pub fn option_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        serde_json::Value::Bool(value) => Some(value.to_string()),
        serde_json::Value::Array(values) => values
            .iter()
            .map(option_value_to_string)
            .collect::<Option<Vec<_>>>()
            .map(|values| values.join("\n")),
        serde_json::Value::Null | serde_json::Value::Object(_) => None,
    }
}

impl DownloadOptions {
    /// Whether this source should use the C++ memory pre-download semantics.
    ///
    /// Either metadata option can request memory-backed handling. This keeps
    /// the decision at the source-download boundary while the post-download
    /// handler still decides whether the bytes are BitTorrent or Metalink.
    pub fn uses_memory_download(&self) -> bool {
        self.follow_torrent.is_some_and(FollowMode::is_memory)
            || self.follow_metalink.is_some_and(FollowMode::is_memory)
    }

    /// Build per-download options from typed configuration values.
    ///
    /// Configuration managers use [`OptionValue`](crate::config::OptionValue)
    /// while session files and RPC option maps use strings. Converting both
    /// through this seam keeps the download engine independent of the source
    /// of the options and gives every caller the same default handling.
    pub fn from_option_values(
        options: &std::collections::HashMap<String, crate::config::OptionValue>,
    ) -> Self {
        let string_options = options
            .iter()
            .filter(|(_, value)| !value.is_none())
            .map(|(key, value)| (key.clone(), value.to_string()))
            .collect();
        Self::from_option_strings(&string_options)
    }

    /// Build per-download options from an RPC option map.
    ///
    /// aria2's JSON-RPC and XML-RPC interfaces use strings for option values;
    /// arrays are accepted for cumulative options such as `header` and are
    /// joined with newlines before entering the shared string parser. Numeric
    /// and boolean JSON values are accepted as a harmless extension for
    /// existing Rust clients, then canonicalized to the same string form.
    pub fn from_rpc_options(
        options: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Self {
        Self::try_from_rpc_options(options).unwrap_or_default()
    }

    /// Fallibly build per-download options from an RPC option map.
    ///
    /// The registry is the validation seam for task creation. Unknown option
    /// names remain ignored, matching aria2's RPC option gatherer, while
    /// known options must pass the same type, range, and enum checks as the
    /// configuration path. The infallible [`Self::from_rpc_options`] helper is
    /// retained for compatibility with older in-process callers; external
    /// adapters must use this method so invalid values cannot become defaults.
    pub fn try_from_rpc_options(
        options: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<Self, String> {
        let registry = crate::config::OptionRegistry::new();
        let mut string_options = std::collections::HashMap::with_capacity(options.len());
        for (key, value) in options {
            if registry.get(key).is_none() {
                continue;
            }
            registry
                .parse_rpc_value(key, value)
                .map_err(|error| format!("Option '{}': {}", key, error))?;
            let value = option_value_to_string(value)
                .ok_or_else(|| format!("Option '{}' must be a string", key))?;
            string_options.insert(key.clone(), value);
        }
        Ok(Self::from_option_strings(&string_options))
    }

    /// Build per-download options from aria2's kebab-case option map.
    ///
    /// This is the shared conversion seam for CLI/session/FFI callers. The
    /// input intentionally contains strings because that is the wire format
    /// used by aria2 configuration files and the original C++ API. Invalid
    /// values fall back to the type's default, while validation of user-facing
    /// configuration remains the responsibility of `ConfigManager`.
    pub fn from_option_strings(options: &std::collections::HashMap<String, String>) -> Self {
        let positive_u16 = |key: &str| {
            options
                .get(key)
                .and_then(|v| v.parse::<u16>().ok())
                .filter(|value| *value > 0)
        };
        let positive_size_u64 = |key: &str| {
            options
                .get(key)
                .map(|v| OptionValue::parse_size_str(v))
                .filter(|value| *value > 0)
        };
        let positive_u64 = |key: &str| {
            options
                .get(key)
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|value| *value > 0)
        };

        Self {
            split: positive_u16("split"),
            max_connection_per_server: positive_u16("max-connection-per-server"),
            max_download_limit: positive_size_u64("max-download-limit"),
            max_upload_limit: positive_size_u64("max-upload-limit"),
            dir: options.get("dir").cloned(),
            out: options.get("out").cloned(),
            file_allocation: options.get("file-allocation").cloned(),
            continue_download: options
                .get("continue")
                .map(|v| v == "true")
                .unwrap_or(false),
            allow_overwrite: options
                .get("allow-overwrite")
                .map(|v| v == "true")
                .unwrap_or(false),
            auto_file_renaming: options
                .get("auto-file-renaming")
                .map(|v| v == "true")
                .unwrap_or(true),
            always_resume: options
                .get("always-resume")
                .map(|v| v == "true")
                .unwrap_or(true),
            max_resume_failure_tries: options
                .get("max-resume-failure-tries")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0),
            remove_control_file: options
                .get("remove-control-file")
                .map(|v| v == "true")
                .unwrap_or(false),
            mmap_threshold: positive_size_u64("mmap-threshold"),
            secure_falloc: options
                .get("secure-falloc")
                .map(|v| v == "true")
                .unwrap_or(false),
            check_integrity: options
                .get("check-integrity")
                .map(|v| v == "true")
                .unwrap_or(false)
                || options
                    .get("hash-check-only")
                    .map(|v| v == "true")
                    .unwrap_or(false),
            hash_check_only: options
                .get("hash-check-only")
                .map(|v| v == "true")
                .unwrap_or(false),
            seed_time: options.get("seed-time").and_then(|v| v.parse::<f64>().ok()),
            seed_ratio: options
                .get("seed-ratio")
                .and_then(|v| v.parse::<f64>().ok()),
            checksum: options.get("checksum").and_then(|v| {
                v.split_once('=')
                    .map(|(algo, hash)| (algo.trim().to_string(), hash.trim().to_string()))
            }),
            cookie_file: options
                .get("load-cookies")
                .or_else(|| options.get("cookie-file"))
                .cloned(),
            cookies: options
                .get("cookie")
                .or_else(|| options.get("cookies"))
                .cloned(),
            bt_max_peers: options
                .get("bt-max-peers")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(55),
            bt_force_encrypt: options
                .get("bt-force-encrypt")
                .or_else(|| options.get("bt-force-encryption"))
                .map(|v| v == "true")
                .unwrap_or(false),
            bt_require_crypto: options
                .get("bt-require-crypto")
                .map(|v| v == "true")
                .unwrap_or(false),
            enable_dht: options
                .get("enable-dht")
                .map(|v| v != "false")
                .unwrap_or(true),
            dht_listen_port: options.get("dht-listen-port").cloned(),
            index_out: options.get("index-out").cloned(),
            dht_entry_point: options.get("dht-entry-point").and_then(|v| {
                let entries = v
                    .split(',')
                    .map(str::trim)
                    .filter(|entry| !entry.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                (!entries.is_empty()).then_some(entries)
            }),
            bt_tracker: options.get("bt-tracker").and_then(|v| {
                let entries = v
                    .split([',', '\n'])
                    .map(str::trim)
                    .filter(|entry| !entry.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                (!entries.is_empty()).then_some(entries)
            }),
            enable_public_trackers: options
                .get("enable-public-trackers")
                .map(|v| v != "false")
                .unwrap_or(true),
            bt_piece_selection_strategy: options
                .get("bt-piece-selection-strategy")
                .cloned()
                .unwrap_or_else(|| crate::constants::DEFAULT_PIECE_STRATEGY.to_string()),
            bt_endgame_threshold: options
                .get("bt-endgame-threshold")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(crate::constants::DEFAULT_BT_ENDGAME_THRESHOLD as u32),
            max_retries: options
                .get("max-retries")
                .or_else(|| options.get("max-tries"))
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(crate::constants::DEFAULT_MAX_RETRIES),
            retry_wait: options
                .get("retry-wait")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(crate::constants::DEFAULT_RETRY_WAIT_SECS),
            http_proxy: options.get("http-proxy").cloned(),
            http_proxy_user: options.get("http-proxy-user").cloned(),
            http_proxy_passwd: options.get("http-proxy-passwd").cloned(),
            all_proxy: options.get("all-proxy").cloned(),
            all_proxy_user: options.get("all-proxy-user").cloned(),
            all_proxy_passwd: options.get("all-proxy-passwd").cloned(),
            https_proxy: options.get("https-proxy").cloned(),
            https_proxy_user: options.get("https-proxy-user").cloned(),
            https_proxy_passwd: options.get("https-proxy-passwd").cloned(),
            ftp_proxy: options.get("ftp-proxy").cloned(),
            ftp_proxy_user: options.get("ftp-proxy-user").cloned(),
            ftp_proxy_passwd: options.get("ftp-proxy-passwd").cloned(),
            no_proxy: options.get("no-proxy").cloned(),
            dht_file_path: options.get("dht-file-path").cloned(),
            bt_max_upload_slots: options
                .get("bt-max-upload-slots")
                .and_then(|v| v.parse::<u32>().ok()),
            bt_optimistic_unchoke_interval: options
                .get("bt-optimistic-unchoke-interval")
                .and_then(|v| v.parse::<u64>().ok()),
            bt_snubbed_timeout: options
                .get("bt-snubbed-timeout")
                .and_then(|v| v.parse::<u64>().ok()),
            bt_prioritize_piece: options
                .get("bt-prioritize-piece")
                .cloned()
                .unwrap_or_default(),
            bt_detach_seed_only: options
                .get("bt-detach-seed-only")
                .map(|v| v == "true")
                .unwrap_or(false),
            enable_utp: options
                .get("enable-utp")
                .map(|v| v == "true")
                .unwrap_or(false),
            utp_listen_port: positive_u16("utp-listen-port"),
            header: options
                .get("header")
                .map(|v| {
                    v.split([',', '\n'])
                        .map(str::trim)
                        .filter(|entry| !entry.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            user_agent: options.get("user-agent").cloned(),
            referer: options.get("referer").cloned(),
            enable_http_keep_alive: options
                .get("enable-http-keep-alive")
                .map(|v| v != "false")
                .unwrap_or(true),
            enable_http_pipelining: options
                .get("enable-http-pipelining")
                .map(|v| v == "true")
                .unwrap_or(false),
            http_accept_gzip: options
                .get("http-accept-gzip")
                .map(|v| v == "true")
                .unwrap_or(false),
            http_no_cache: options
                .get("http-no-cache")
                .map(|v| v == "true")
                .unwrap_or(false),
            use_head: options
                .get("use-head")
                .map(|v| v == "true")
                .unwrap_or(false),
            no_want_digest_header: options
                .get("no-want-digest-header")
                .map(|v| v == "true")
                .unwrap_or(false),
            check_certificate: options
                .get("check-certificate")
                .map(|v| v != "false")
                .unwrap_or(true),
            ca_certificate: options.get("ca-certificate").cloned(),
            min_tls_version: options.get("min-tls-version").cloned(),
            metalink_version: options.get("metalink-version").cloned(),
            metalink_language: options.get("metalink-language").cloned(),
            metalink_os: options.get("metalink-os").cloned(),
            metalink_location: options.get("metalink-location").cloned(),
            metalink_preferred_protocol: options.get("metalink-preferred-protocol").cloned(),
            select_file: options.get("select-file").cloned(),
            piece_length: positive_size_u64("piece-length"),
            metalink_enable_unique_protocol: options
                .get("metalink-enable-unique-protocol")
                .map(|v| v != "false")
                .unwrap_or(true),
            timeout: positive_u64("timeout"),
            connect_timeout: positive_u64("connect-timeout"),
            startup_idle_time: positive_u64("startup-idle-time"),
            lowest_speed_limit: positive_size_u64("lowest-speed-limit"),
            ftp_pasv: options
                .get("ftp-pasv")
                .map(|v| v != "false")
                .unwrap_or(true),
            remote_time: options
                .get("remote-time")
                .map(|v| v == "true")
                .unwrap_or(false),
            dry_run: options.get("dry-run").map(|v| v == "true").unwrap_or(false),
            ftp_reuse_connection: options
                .get("ftp-reuse-connection")
                .map(|v| v != "false")
                .unwrap_or(true),
            realtime_chunk_checksum: options
                .get("realtime-chunk-checksum")
                .map(|v| v != "false")
                .unwrap_or(true),
            bt_stop_timeout: options
                .get("bt-stop-timeout")
                .and_then(|v| v.parse::<u64>().ok()),
            disable_ipv6: options
                .get("disable-ipv6")
                .map(|v| v == "true")
                .unwrap_or(false),
            listen_port: options.get("listen-port").cloned(),
            bt_enable_lpd: options
                .get("bt-enable-lpd")
                .map(|v| v == "true")
                .unwrap_or(false),
            bt_lpd_interface: options.get("bt-lpd-interface").cloned(),
            enable_rpc: options
                .get("enable-rpc")
                .map(|v| v == "true")
                .unwrap_or(false),
            pause: options.get("pause").map(|v| v == "true").unwrap_or(false),
            follow_torrent: options
                .get("follow-torrent")
                .and_then(|v| FollowMode::parse(v)),
            follow_metalink: options
                .get("follow-metalink")
                .and_then(|v| FollowMode::parse(v)),
            http_auth_challenge: options
                .get("http-auth-challenge")
                .map(|v| v == "true")
                .unwrap_or(false),
            http_user: options.get("http-user").cloned(),
            http_passwd: options.get("http-passwd").cloned(),
            ftp_user: options.get("ftp-user").cloned(),
            ftp_passwd: options.get("ftp-passwd").cloned(),
            ssh_host_key_md: options.get("ssh-host-key-md").cloned(),
            no_netrc: options
                .get("no-netrc")
                .map(|v| v == "true")
                .unwrap_or(false),
            netrc_path: options.get("netrc-path").cloned(),
            conditional_get: options
                .get("conditional-get")
                .map(|v| v == "true")
                .unwrap_or(false),
            on_download_start: options.get("on-download-start").cloned(),
            on_download_complete: options.get("on-download-complete").cloned(),
            on_download_error: options.get("on-download-error").cloned(),
            on_download_pause: options.get("on-download-pause").cloned(),
            on_download_stop: options.get("on-download-stop").cloned(),
            on_bt_download_complete: options.get("on-bt-download-complete").cloned(),
        }
    }

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

    /// Build the internal HTTP request policy shared by every HTTP request
    /// path. The option names and defaults remain exposed through the aria2
    /// compatible configuration/RPC surfaces.
    pub fn http_request_policy(&self) -> crate::http::HttpRequestPolicy {
        crate::http::HttpRequestPolicy::new(
            self.parsed_headers(),
            self.http_accept_gzip,
            self.http_no_cache,
            !self.no_want_digest_header,
            self.enable_http_keep_alive,
            self.enable_http_pipelining,
        )
    }

    /// Resolve proxy credentials using aria2's protocol-specific precedence.
    ///
    /// A protocol-specific credential overrides the corresponding
    /// `all-proxy-*` value. The `all` selector is used when constructing the
    /// fallback proxy matcher itself.
    pub(crate) fn proxy_credentials_for_scheme(
        &self,
        scheme: &str,
    ) -> (Option<String>, Option<String>) {
        let (user, passwd) = match scheme {
            "https" => (&self.https_proxy_user, &self.https_proxy_passwd),
            "http" => (&self.http_proxy_user, &self.http_proxy_passwd),
            _ => (&self.all_proxy_user, &self.all_proxy_passwd),
        };

        (
            user.clone().or_else(|| self.all_proxy_user.clone()),
            passwd.clone().or_else(|| self.all_proxy_passwd.clone()),
        )
    }
}

/// Case-insensitive check whether a `(name, value)` header list already contains
/// an entry with the given name.
fn has_header(headers: &[(String, String)], name: &str) -> bool {
    headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::{DownloadOptions, FollowMode};
    use std::collections::HashMap;

    #[test]
    fn follow_mode_preserves_all_wire_values() {
        assert_eq!(FollowMode::parse("true"), Some(FollowMode::Follow));
        assert_eq!(FollowMode::parse("false"), Some(FollowMode::Disabled));
        assert_eq!(FollowMode::parse("mem"), Some(FollowMode::Memory));
        assert_eq!(FollowMode::parse("invalid"), None);
        assert_eq!(FollowMode::from_bool(true), FollowMode::Follow);
        assert_eq!(FollowMode::from_bool(false), FollowMode::Disabled);
        assert_eq!(FollowMode::Memory.as_str(), "mem");
    }

    #[test]
    fn option_map_keeps_memory_follow_mode() {
        let mut values = HashMap::new();
        values.insert("follow-torrent".to_string(), "mem".to_string());
        values.insert("follow-metalink".to_string(), "false".to_string());

        let options = DownloadOptions::from_option_strings(&values);
        assert_eq!(options.follow_torrent, Some(FollowMode::Memory));
        assert_eq!(options.follow_metalink, Some(FollowMode::Disabled));
        assert!(options.uses_memory_download());
    }

    #[test]
    fn proxy_credentials_prefer_protocol_specific_values() {
        let options = DownloadOptions {
            http_proxy_user: Some("http-user".to_string()),
            http_proxy_passwd: Some("http-pass".to_string()),
            https_proxy_user: Some("https-user".to_string()),
            all_proxy_user: Some("all-user".to_string()),
            all_proxy_passwd: Some("all-pass".to_string()),
            ..DownloadOptions::default()
        };

        assert_eq!(
            options.proxy_credentials_for_scheme("http"),
            (Some("http-user".to_string()), Some("http-pass".to_string()))
        );
        assert_eq!(
            options.proxy_credentials_for_scheme("https"),
            (Some("https-user".to_string()), Some("all-pass".to_string()))
        );
        assert_eq!(
            options.proxy_credentials_for_scheme("all"),
            (Some("all-user".to_string()), Some("all-pass".to_string()))
        );
    }

    #[cfg(feature = "bittorrent")]
    #[test]
    fn rpc_option_map_uses_aria2_wire_strings() {
        let mut values = HashMap::new();
        values.insert("max-download-limit".to_string(), serde_json::json!("100K"));
        values.insert("max-retries".to_string(), serde_json::json!("7"));
        values.insert("follow-torrent".to_string(), serde_json::json!("mem"));
        values.insert(
            "index-out".to_string(),
            serde_json::json!(["1=first.iso", "2=second.iso"]),
        );
        values.insert(
            "header".to_string(),
            serde_json::json!(["X-One: 1", "X-Two: 2"]),
        );

        let options = DownloadOptions::from_rpc_options(&values);

        assert_eq!(options.max_download_limit, Some(100 * 1024));
        assert_eq!(options.max_retries, 7);
        assert_eq!(options.follow_torrent, Some(FollowMode::Memory));
        assert_eq!(
            options.index_out.as_deref(),
            Some("1=first.iso\n2=second.iso")
        );
        assert_eq!(options.header, vec!["X-One: 1", "X-Two: 2"]);
    }

    #[test]
    fn rpc_option_map_rejects_invalid_registered_values() {
        let mut values = HashMap::new();
        values.insert(
            "metalink-preferred-protocol".to_string(),
            serde_json::json!("gopher"),
        );

        let error = DownloadOptions::try_from_rpc_options(&values)
            .expect_err("invalid enum values must not fall back to defaults");
        assert!(error.contains("metalink-preferred-protocol"));
    }

    #[test]
    fn continue_option_defaults_to_false_and_accepts_explicit_true() {
        assert!(!DownloadOptions::from_option_strings(&HashMap::new()).continue_download);

        let mut values = HashMap::new();
        values.insert("continue".to_string(), "true".to_string());
        assert!(DownloadOptions::from_option_strings(&values).continue_download);
    }
}
