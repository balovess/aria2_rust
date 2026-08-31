//! Runtime option updates and rate limiter management.
//!
//! Implements `RequestGroup::update_option()` for dynamically changing
//! download options at runtime (e.g. via `aria2.changeOption`), and
//! the `set_rate_limiter` / `set_download_context` methods.

use std::collections::HashMap;
use std::sync::Arc;

use crate::rate_limiter::RateLimiter;
use crate::util::rwlock_ext::RwLockRecover;

fn rpc_option_string(value: &serde_json::Value, key: &str) -> Result<String, String> {
    super::options::option_value_to_string(value)
        .ok_or_else(|| format!("Option '{}' must be a string", key))
}

fn rpc_option_u64(value: &serde_json::Value, key: &str) -> Result<u64, String> {
    rpc_option_string(value, key)?
        .parse()
        .map_err(|_| format!("Option '{}' must be a non-negative integer", key))
}

fn rpc_option_size(value: &serde_json::Value, key: &str) -> Result<u64, String> {
    let raw = rpc_option_string(value, key)?;
    crate::config::OptionValue::parse_size_str_checked(&raw)
        .map_err(|error| format!("Option '{}': {}", key, error))
}

fn rpc_option_f64(value: &serde_json::Value, key: &str) -> Result<f64, String> {
    let number = rpc_option_string(value, key)?
        .parse::<f64>()
        .map_err(|_| format!("Option '{}' must be a number", key))?;
    if number.is_finite() {
        Ok(number)
    } else {
        Err(format!("Option '{}' must be a finite number", key))
    }
}

fn rpc_option_bool(value: &serde_json::Value, key: &str) -> Result<bool, String> {
    match rpc_option_string(value, key)?.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("Option '{}' must be either 'true' or 'false'", key)),
    }
}

fn rpc_option_u16(value: &serde_json::Value, key: &str) -> Result<u16, String> {
    let value = rpc_option_u64(value, key)?;
    u16::try_from(value).map_err(|_| format!("Option '{}' is too large", key))
}

fn rpc_option_u32(value: &serde_json::Value, key: &str) -> Result<u32, String> {
    let value = rpc_option_u64(value, key)?;
    u32::try_from(value).map_err(|_| format!("Option '{}' is too large", key))
}

fn rpc_option_list(value: &serde_json::Value, key: &str) -> Result<Vec<String>, String> {
    let values = match value {
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| rpc_option_string(value, key))
            .collect::<Result<Vec<_>, _>>()?,
        _ => rpc_option_string(value, key)?
            .split([',', '\n'])
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(str::to_string)
            .collect(),
    };
    if values.is_empty() {
        return Err(format!("Option '{}' must not be empty", key));
    }
    Ok(values)
}

/// Runtime option changes partitioned by the lifecycle phase in which aria2
/// applies them. This is an internal core seam shared by RPC and C adapters.
#[derive(Debug, Default)]
pub(crate) struct RuntimeOptionChanges {
    pub(crate) immediate: HashMap<String, serde_json::Value>,
    pub(crate) pending: HashMap<String, serde_json::Value>,
}

fn apply_rpc_option(
    opts: &mut super::DownloadOptions,
    key: &str,
    value: &serde_json::Value,
) -> Result<bool, String> {
    match key {
        "split" => {
            let value = rpc_option_u16(value, key)?;
            if value == 0 {
                return Err(format!("Option '{}' must be greater than zero", key));
            }
            opts.split = Some(value);
            Ok(true)
        }
        "max-download-limit" => {
            opts.max_download_limit = Some(rpc_option_size(value, key)?);
            Ok(true)
        }
        "max-upload-limit" => {
            opts.max_upload_limit = Some(rpc_option_size(value, key)?);
            Ok(true)
        }
        "max-tries" | "max-retries" | "max-resume-failure-tries" => {
            let value = rpc_option_u32(value, key)?;
            if key == "max-resume-failure-tries" {
                opts.max_resume_failure_tries = value;
            } else {
                opts.max_retries = value;
            }
            Ok(true)
        }
        "retry-wait" => {
            let value = rpc_option_u64(value, key)?;
            if value > 600 {
                return Err(format!("Option '{}' must be between 0 and 600", key));
            }
            opts.retry_wait = value;
            Ok(true)
        }
        "allow-overwrite"
        | "allow-piece-length-change"
        | "always-resume"
        | "auto-file-renaming"
        | "async-dns"
        | "enable-mmap"
        | "parameterized-uri"
        | "reuse-uri"
        | "continue"
        | "remove-control-file"
        | "enable-http-keep-alive"
        | "enable-http-pipelining"
        | "http-accept-gzip"
        | "http-no-cache"
        | "use-head"
        | "no-want-digest-header"
        | "pause"
        | "pause-metadata"
        | "force-save"
        | "save-not-found"
        | "rpc-save-upload-metadata"
        | "content-disposition-default-utf8"
        | "bt-load-saved-metadata"
        | "bt-metadata-only"
        | "bt-save-metadata"
        | "bt-enable-web-seed"
        | "enable-peer-exchange" => {
            let value = rpc_option_bool(value, key)?;
            match key {
                "allow-overwrite" => opts.allow_overwrite = value,
                "allow-piece-length-change" => opts.allow_piece_length_change = value,
                "always-resume" => opts.always_resume = value,
                "auto-file-renaming" => opts.auto_file_renaming = value,
                "async-dns" => opts.async_dns = value,
                "enable-mmap" => opts.enable_mmap = value,
                "parameterized-uri" => opts.parameterized_uri = value,
                "reuse-uri" => opts.reuse_uri = value,
                "continue" => opts.continue_download = value,
                "remove-control-file" => opts.remove_control_file = value,
                "enable-http-keep-alive" => opts.enable_http_keep_alive = value,
                "enable-http-pipelining" => opts.enable_http_pipelining = value,
                "http-accept-gzip" => opts.http_accept_gzip = value,
                "http-no-cache" => opts.http_no_cache = value,
                "use-head" => opts.use_head = value,
                "no-want-digest-header" => opts.no_want_digest_header = value,
                "pause" => opts.pause = value,
                "pause-metadata" => opts.pause_metadata = value,
                "force-save" => opts.force_save = value,
                "save-not-found" => opts.save_not_found = value,
                "rpc-save-upload-metadata" => opts.rpc_save_upload_metadata = value,
                "content-disposition-default-utf8" => opts.content_disposition_default_utf8 = value,
                "bt-load-saved-metadata" => opts.bt_load_saved_metadata = value,
                "bt-metadata-only" => opts.bt_metadata_only = value,
                "bt-save-metadata" => opts.bt_save_metadata = value,
                "bt-enable-web-seed" => opts.bt_enable_web_seed = value,
                "enable-peer-exchange" => opts.enable_peer_exchange = value,
                _ => unreachable!("boolean option handled above"),
            }
            Ok(true)
        }
        "header" => {
            opts.header = match value {
                serde_json::Value::Array(values) => values
                    .iter()
                    .map(|value| rpc_option_string(value, key))
                    .collect::<Result<Vec<_>, _>>()?,
                serde_json::Value::String(value) => value
                    .split('\n')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .collect(),
                _ => return Err(format!("Option '{}' must be a string or array", key)),
            };
            Ok(true)
        }
        "user-agent" | "referer" | "dir" | "out" | "file-allocation" | "cookie-file"
        | "cookies" | "dht-file-path" | "http-proxy" | "http-proxy-user" | "http-proxy-passwd"
        | "all-proxy" | "all-proxy-user" | "all-proxy-passwd" | "https-proxy"
        | "https-proxy-user" | "https-proxy-passwd" | "ftp-proxy" | "ftp-proxy-user"
        | "ftp-proxy-passwd" | "no-proxy" => {
            let value = rpc_option_string(value, key)?;
            match key {
                "user-agent" => opts.user_agent = Some(value),
                "referer" => opts.referer = Some(value),
                "dir" => opts.dir = Some(value),
                "out" => opts.out = Some(value),
                "file-allocation" => opts.file_allocation = Some(value),
                "cookie-file" => opts.cookie_file = Some(value),
                "cookies" => opts.cookies = Some(value),
                "dht-file-path" => opts.dht_file_path = Some(value),
                "http-proxy" => opts.http_proxy = Some(value),
                "http-proxy-user" => opts.http_proxy_user = Some(value),
                "http-proxy-passwd" => opts.http_proxy_passwd = Some(value),
                "all-proxy" => opts.all_proxy = Some(value),
                "all-proxy-user" => opts.all_proxy_user = Some(value),
                "all-proxy-passwd" => opts.all_proxy_passwd = Some(value),
                "https-proxy" => opts.https_proxy = Some(value),
                "https-proxy-user" => opts.https_proxy_user = Some(value),
                "https-proxy-passwd" => opts.https_proxy_passwd = Some(value),
                "ftp-proxy" => opts.ftp_proxy = Some(value),
                "ftp-proxy-user" => opts.ftp_proxy_user = Some(value),
                "ftp-proxy-passwd" => opts.ftp_proxy_passwd = Some(value),
                "no-proxy" => opts.no_proxy = Some(value),
                _ => unreachable!("string option handled above"),
            }
            Ok(true)
        }
        "max-connection-per-server" => {
            let value = rpc_option_u16(value, key)?;
            if value == 0 {
                return Err(format!("Option '{}' must be greater than zero", key));
            }
            opts.max_connection_per_server = Some(value);
            Ok(true)
        }
        "max-file-not-found" => {
            opts.max_file_not_found = rpc_option_u32(value, key)?;
            Ok(true)
        }
        "bt-max-peers" => {
            opts.bt_max_peers = usize::try_from(rpc_option_u64(value, key)?)
                .map_err(|_| format!("Option '{}' is too large", key))?;
            Ok(true)
        }
        "bt-max-open-files" => {
            opts.bt_max_open_files = usize::try_from(rpc_option_u64(value, key)?)
                .map_err(|_| format!("Option '{}' is too large", key))?;
            if opts.bt_max_open_files == 0 {
                return Err(format!("Option '{}' must be greater than zero", key));
            }
            Ok(true)
        }
        "bt-max-upload-slots" => {
            opts.bt_max_upload_slots = Some(rpc_option_u32(value, key)?);
            Ok(true)
        }
        "bt-request-peer-speed-limit" => {
            opts.bt_request_peer_speed_limit = rpc_option_size(value, key)?;
            Ok(true)
        }
        "bt-tracker-connect-timeout"
        | "bt-tracker-interval"
        | "bt-tracker-timeout"
        | "bt-tracker-stopped-timeout" => {
            let value = rpc_option_u64(value, key)?;
            match key {
                "bt-tracker-connect-timeout" => opts.bt_tracker_connect_timeout = value,
                "bt-tracker-interval" => opts.bt_tracker_interval = value,
                "bt-tracker-timeout" => opts.bt_tracker_timeout = value,
                "bt-tracker-stopped-timeout" => opts.bt_tracker_stopped_timeout = value,
                _ => unreachable!("tracker duration option handled above"),
            }
            Ok(true)
        }
        "bt-snubbed-timeout" => {
            opts.bt_snubbed_timeout = Some(rpc_option_u64(value, key)?);
            Ok(true)
        }
        "bt-keep-alive-interval"
        | "bt-timeout"
        | "bt-request-timeout"
        | "peer-connection-timeout"
        | "dht-message-timeout" => {
            let value = rpc_option_u64(value, key)?;
            if value == 0 {
                return Err(format!("Option '{}' must be greater than zero", key));
            }
            match key {
                "bt-keep-alive-interval" => opts.bt_keep_alive_interval = value,
                "bt-timeout" => opts.bt_timeout = value,
                "bt-request-timeout" => opts.bt_request_timeout = value,
                "peer-connection-timeout" => opts.peer_connection_timeout = value,
                "dht-message-timeout" => opts.dht_message_timeout = value,
                _ => unreachable!("BitTorrent duration option handled above"),
            }
            Ok(true)
        }
        "bt-optimistic-unchoke-interval" => {
            opts.bt_optimistic_unchoke_interval = Some(rpc_option_u64(value, key)?);
            Ok(true)
        }
        "bt-endgame-threshold" => {
            opts.bt_endgame_threshold = rpc_option_u32(value, key)?;
            Ok(true)
        }
        "seed-time" | "seed-ratio" => {
            let value = rpc_option_f64(value, key)?;
            if value < 0.0 {
                return Err(format!("Option '{}' must not be negative", key));
            }
            if key == "seed-time" {
                opts.seed_time = Some(value);
            } else {
                opts.seed_ratio = Some(value);
            }
            Ok(true)
        }
        "bt-detach-seed-only" => {
            opts.bt_detach_seed_only = rpc_option_bool(value, key)?;
            Ok(true)
        }
        "mmap-threshold" => {
            opts.mmap_threshold = Some(rpc_option_size(value, key)?);
            Ok(true)
        }
        "secure-falloc" => {
            opts.secure_falloc = rpc_option_bool(value, key)?;
            Ok(true)
        }
        "checksum" => {
            let value = rpc_option_string(value, key)?;
            let (algorithm, digest) = value
                .split_once('=')
                .filter(|(algorithm, digest)| !algorithm.is_empty() && !digest.is_empty())
                .ok_or_else(|| format!("Option '{}' must be in HASH=VALUE form", key))?;
            opts.checksum = Some((algorithm.to_string(), digest.to_string()));
            Ok(true)
        }
        "bt-force-encryption" | "bt-force-encrypt" => {
            opts.bt_force_encrypt = rpc_option_bool(value, key)?;
            Ok(true)
        }
        "bt-require-crypto" => {
            opts.bt_require_crypto = rpc_option_bool(value, key)?;
            Ok(true)
        }
        "enable-dht" => {
            opts.enable_dht = rpc_option_bool(value, key)?;
            Ok(true)
        }
        "enable-dht6" => {
            opts.enable_dht6 = rpc_option_bool(value, key)?;
            Ok(true)
        }
        "dht-listen-port" => {
            opts.dht_listen_port = Some(rpc_option_string(value, key)?);
            Ok(true)
        }
        "dht-entry-point" => {
            opts.dht_entry_point = match value {
                serde_json::Value::Array(values) => Some(
                    values
                        .iter()
                        .map(|value| rpc_option_string(value, key))
                        .collect::<Result<Vec<_>, _>>()?,
                ),
                _ => Some(vec![rpc_option_string(value, key)?]),
            };
            Ok(true)
        }
        "dht-entry-point-port" | "dht-entry-point-port6" => {
            let value = rpc_option_u16(value, key)?;
            match key {
                "dht-entry-point-port" => opts.dht_entry_point_port = Some(value),
                "dht-entry-point-port6" => opts.dht_entry_point_port6 = Some(value),
                _ => unreachable!("DHT bootstrap port option handled above"),
            }
            Ok(true)
        }
        "enable-public-trackers" => {
            opts.enable_public_trackers = rpc_option_bool(value, key)?;
            Ok(true)
        }
        "bt-piece-selection-strategy" => {
            opts.bt_piece_selection_strategy = rpc_option_string(value, key)?;
            Ok(true)
        }
        "bt-prioritize-piece" => {
            opts.bt_prioritize_piece = rpc_option_string(value, key)?;
            Ok(true)
        }
        "enable-utp" => {
            opts.enable_utp = rpc_option_bool(value, key)?;
            Ok(true)
        }
        "utp-listen-port" => {
            let value = rpc_option_u16(value, key)?;
            opts.utp_listen_port = Some(value);
            Ok(true)
        }
        "check-integrity"
        | "conditional-get"
        | "dry-run"
        | "ftp-pasv"
        | "ftp-reuse-connection"
        | "no-netrc"
        | "realtime-chunk-checksum"
        | "remote-time"
        | "bt-enable-lpd"
        | "http-auth-challenge" => {
            let value = rpc_option_bool(value, key)?;
            match key {
                "check-integrity" => opts.check_integrity = value,
                "conditional-get" => opts.conditional_get = value,
                "dry-run" => opts.dry_run = value,
                "ftp-pasv" => opts.ftp_pasv = value,
                "ftp-reuse-connection" => opts.ftp_reuse_connection = value,
                "no-netrc" => opts.no_netrc = value,
                "realtime-chunk-checksum" => opts.realtime_chunk_checksum = value,
                "remote-time" => opts.remote_time = value,
                "bt-enable-lpd" => opts.bt_enable_lpd = value,
                "http-auth-challenge" => opts.http_auth_challenge = value,
                _ => unreachable!("boolean option handled above"),
            }
            Ok(true)
        }
        "hash-check-only" => {
            let value = rpc_option_bool(value, key)?;
            opts.hash_check_only = value;
            if value {
                opts.check_integrity = true;
            }
            Ok(true)
        }
        "bt-enable-hook-after-hash-check" => {
            opts.bt_enable_hook_after_hash_check = rpc_option_bool(value, key)?;
            Ok(true)
        }
        "bt-hash-check-seed" => {
            opts.bt_hash_check_seed = rpc_option_bool(value, key)?;
            Ok(true)
        }
        "bt-seed-unverified" => {
            opts.bt_seed_unverified = rpc_option_bool(value, key)?;
            Ok(true)
        }
        "bt-remove-unselected-file" => {
            opts.bt_remove_unselected_file = rpc_option_bool(value, key)?;
            Ok(true)
        }
        "timeout" | "connect-timeout" | "bt-stop-timeout" => {
            let value = rpc_option_u64(value, key)?;
            match key {
                "timeout" => opts.timeout = Some(value),
                "connect-timeout" => opts.connect_timeout = Some(value),
                "bt-stop-timeout" => opts.bt_stop_timeout = Some(value),
                _ => unreachable!("duration option handled above"),
            }
            Ok(true)
        }
        "lowest-speed-limit"
        | "piece-length"
        | "min-split-size"
        | "disk-cache"
        | "max-mmap-limit"
        | "no-file-allocation-limit" => {
            let value = rpc_option_size(value, key)?;
            match key {
                "lowest-speed-limit" => opts.lowest_speed_limit = Some(value),
                "piece-length" => opts.piece_length = Some(value),
                "min-split-size" => opts.min_split_size = Some(value),
                "disk-cache" => opts.disk_cache = Some(value),
                "max-mmap-limit" => opts.max_mmap_limit = Some(value),
                "no-file-allocation-limit" => opts.no_file_allocation_limit = Some(value),
                _ => unreachable!("size option handled above"),
            }
            Ok(true)
        }
        "metalink-version"
        | "metalink-language"
        | "metalink-os"
        | "metalink-location"
        | "metalink-base-uri"
        | "metalink-preferred-protocol"
        | "select-file"
        | "index-out"
        | "listen-port"
        | "http-user"
        | "http-passwd"
        | "ftp-user"
        | "ftp-passwd"
        | "ssh-host-key-md"
        | "bt-external-ip"
        | "bt-min-crypto-level"
        | "ftp-type"
        | "uri-selector"
        | "stream-piece-selector"
        | "proxy-method" => {
            let value = rpc_option_string(value, key)?;
            match key {
                "metalink-version" => opts.metalink_version = Some(value),
                "metalink-language" => opts.metalink_language = Some(value),
                "metalink-os" => opts.metalink_os = Some(value),
                "metalink-location" => opts.metalink_location = Some(value),
                "metalink-base-uri" => opts.metalink_base_uri = Some(value),
                "metalink-preferred-protocol" => opts.metalink_preferred_protocol = Some(value),
                "select-file" => opts.select_file = Some(value),
                "index-out" => opts.index_out = Some(value),
                "listen-port" => opts.listen_port = Some(value),
                "http-user" => opts.http_user = Some(value),
                "http-passwd" => opts.http_passwd = Some(value),
                "ftp-user" => opts.ftp_user = Some(value),
                "ftp-passwd" => opts.ftp_passwd = Some(value),
                "ssh-host-key-md" => opts.ssh_host_key_md = Some(value),
                "bt-external-ip" => opts.bt_external_ip = Some(value),
                "bt-peer-blocklist" => opts.bt_peer_blocklist = Some(value),
                "peer-id-prefix" => opts.peer_id_prefix = value,
                "peer-agent" => opts.peer_agent = value,
                "dht-listen-addr6" => opts.dht_listen_addr6 = Some(value),
                "dht-entry-point-host" => opts.dht_entry_point_host = Some(value),
                "dht-entry-point6" => opts.dht_entry_point6 = Some(value),
                "dht-entry-point-host6" => opts.dht_entry_point_host6 = Some(value),
                "dht-file-path6" => opts.dht_file_path6 = Some(value),
                "dht-listen-addr" => opts.dht_listen_addr = Some(value),
                "bt-min-crypto-level" => opts.bt_min_crypto_level = value,
                "ftp-type" => opts.ftp_type = value,
                "uri-selector" => opts.uri_selector = value,
                "stream-piece-selector" => opts.stream_piece_selector = value,
                "proxy-method" => opts.proxy_method = value,
                _ => unreachable!("string option handled above"),
            }
            Ok(true)
        }
        "metalink-enable-unique-protocol" => {
            opts.metalink_enable_unique_protocol = rpc_option_bool(value, key)?;
            Ok(true)
        }
        "follow-torrent" | "follow-metalink" => {
            let raw = rpc_option_string(value, key)?;
            let mode = super::FollowMode::parse(&raw)
                .ok_or_else(|| format!("Option '{}' must be true, false, or mem", key))?;
            if key == "follow-torrent" {
                opts.follow_torrent = Some(mode);
            } else {
                opts.follow_metalink = Some(mode);
            }
            Ok(true)
        }
        "bt-tracker" => {
            opts.bt_tracker = Some(rpc_option_list(value, key)?);
            Ok(true)
        }
        "bt-exclude-tracker" => {
            opts.bt_exclude_tracker = Some(rpc_option_list(value, key)?);
            Ok(true)
        }
        _ => Ok(false),
    }
}

impl super::RequestGroup {
    // ── Basic Accessors ─────────────────────────────────────────────────

    /// Resolve the effective minimum range split size from the task snapshot,
    /// falling back to the typed execution option when no snapshot exists.
    pub(crate) fn effective_min_split_size(&self) -> u64 {
        self.effective_option_snapshot()
            .and_then(|options| options.get("min-split-size").cloned())
            .and_then(|value| super::options::option_value_to_string(&value))
            .and_then(|value| crate::config::OptionValue::parse_size_str_checked(&value).ok())
            .filter(|value| *value > 0)
            .or_else(|| self.options.min_split_size.filter(|value| *value > 0))
            .unwrap_or(crate::constants::DEFAULT_MIN_SPLIT_SIZE)
    }

    /// Return the group ID.
    pub fn gid(&self) -> super::GroupId {
        self.gid
    }

    /// Return the initial URI list.
    ///
    /// Note: This returns the *initial* URIs provided when the group was
    /// created. For the current remaining/spent URI state, use
    /// `get_remaining_uris()` / `get_spent_uris()` which delegate to
    /// `FileEntry` via `DownloadContext`.
    pub fn uris(&self) -> &[String] {
        &self.uris
    }

    /// Replace the initial URI set before a download context is attached.
    ///
    /// Dependency fallbacks use this to remove the synthetic `bt://` dispatch
    /// URI after torrent metadata failed and continue with direct mirrors.
    pub fn replace_uris(&mut self, uris: Vec<String>) {
        self.uris = uris;
    }

    /// Set a per-group output filename, used by Metalink entries.
    pub fn set_output_name(&self, name: impl Into<String>) {
        *self.output_name.recover_mut() = Some(name.into());
    }

    /// Return the per-group output filename, if configured.
    pub fn output_name(&self) -> Option<String> {
        self.output_name.recover().clone()
    }

    /// Return a reference to the download options.
    pub fn options(&self) -> &super::DownloadOptions {
        &self.options
    }

    /// Cheap clone of the options `Arc` — O(1) refcount bump instead of
    /// deep-cloning all `Vec<String>` fields.
    pub fn options_arc(&self) -> Arc<super::DownloadOptions> {
        Arc::clone(&self.options)
    }

    /// Record the canonical option values that created this task.
    ///
    /// Callers set this while the group is constructed or restored. It is
    /// intentionally separate from runtime overrides so a later global option
    /// change cannot alter the observable task state. Typed fields are
    /// synchronized here; options without a typed field remain in the raw
    /// snapshot for protocol and session consumers.
    pub fn set_option_snapshot(&mut self, options: HashMap<String, serde_json::Value>) {
        let snapshot = crate::config::project_initial_options(options);
        let typed_options = Arc::make_mut(&mut self.options);
        for (key, value) in &snapshot {
            let _ = apply_rpc_option(typed_options, key, value);
        }
        self.option_snapshot = Some(snapshot);
    }

    /// Return the creation snapshot with only already-applied runtime changes
    /// overlaid. Pending changes remain absent until a restart applies them.
    pub fn effective_option_snapshot(&self) -> Option<HashMap<String, serde_json::Value>> {
        let mut options = self.option_snapshot.clone()?;
        options.extend(self.runtime_options());
        Some(crate::config::project_initial_options(options))
    }

    /// Return the configured maximum number of 404 responses for this group.
    ///
    /// `max-file-not-found=0` disables 404 retries and therefore reports the
    /// first response as `RESOURCE_NOT_FOUND`, matching aria2's wire behavior.
    pub fn max_file_not_found(&self) -> u32 {
        self.effective_option_snapshot()
            .and_then(|options| options.get("max-file-not-found").cloned())
            .and_then(|value| super::options::option_value_to_string(&value))
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(self.options.max_file_not_found)
    }

    /// Record a not-found response and return the terminal or retryable code.
    pub fn record_file_not_found(&self) -> super::result_code::DownloadResultCode {
        let count = self
            .file_not_found_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .saturating_add(1);
        let max = self.max_file_not_found();

        if max > 0 && count >= max && self.completed_length() == 0 {
            super::result_code::DownloadResultCode::MaxFileNotFound
        } else {
            super::result_code::DownloadResultCode::ResourceNotFound
        }
    }

    /// Convert the next HTTP 404 response into the public download error.
    pub fn file_not_found_error(&self) -> crate::error::Aria2Error {
        match self.record_file_not_found() {
            super::result_code::DownloadResultCode::MaxFileNotFound => {
                crate::error::Aria2Error::Recoverable(
                    crate::error::RecoverableError::MaxFileNotFound,
                )
            }
            _ => crate::error::Aria2Error::Recoverable(
                crate::error::RecoverableError::ResourceNotFound,
            ),
        }
    }

    /// Return whether another 404 request is permitted by the group option.
    pub fn can_retry_file_not_found(&self) -> bool {
        let max = self.max_file_not_found();
        max > 0
            && (self
                .file_not_found_count
                .load(std::sync::atomic::Ordering::Relaxed)
                < max
                || self.completed_length() > 0)
    }

    // ── Rate Limiter ────────────────────────────────────────────────────

    /// Store a handle to the download's `RateLimiter` so that runtime option
    /// updates (e.g. via `aria2.changeOption`) can dynamically adjust the rate.
    pub fn set_rate_limiter(&self, limiter: RateLimiter) {
        *self.rate_limiter.recover_mut() = Some(limiter);
    }

    /// Store options that take effect when the next command generation starts.
    pub fn set_pending_options(
        &self,
        changes: std::collections::HashMap<String, serde_json::Value>,
    ) {
        if let Ok(mut pending) = self.pending_options.write() {
            pending.extend(changes);
        }
    }

    /// Apply and clear options deferred by `changeOption`.
    pub fn apply_pending_options(&mut self) {
        let changes = self
            .pending_options
            .write()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default();
        for (key, value) in changes {
            self.update_option(&key, value);
        }
    }

    pub fn pending_options(&self) -> std::collections::HashMap<String, serde_json::Value> {
        self.pending_options
            .read()
            .map(|pending| pending.clone())
            .unwrap_or_default()
    }

    /// Return the task-level overrides that have actually been applied.
    pub fn runtime_options(&self) -> std::collections::HashMap<String, serde_json::Value> {
        self.runtime_options
            .read()
            .map(|options| options.clone())
            .unwrap_or_default()
    }

    /// Validate and partition a batch using the same policy for every
    /// external adapter. Waiting and paused groups are reserved; only a
    /// group in the `Active` state receives pending changes.
    pub(crate) fn classify_runtime_options(
        &self,
        changes: HashMap<String, serde_json::Value>,
    ) -> Result<RuntimeOptionChanges, String> {
        let is_running = self.status().is_running();
        let mut classified = RuntimeOptionChanges::default();
        for (key, value) in changes {
            match crate::config::is_option_changeable(&key, is_running) {
                crate::config::ChangeableKind::Immediate => {
                    if Self::validate_option_update(&key, &value)? {
                        classified.immediate.insert(key, value);
                    }
                }
                crate::config::ChangeableKind::Pending => {
                    if Self::validate_option_update(&key, &value)? {
                        classified.pending.insert(key, value);
                    }
                }
                crate::config::ChangeableKind::NotChangeable => {}
            }
        }
        Ok(classified)
    }

    /// Apply a previously classified immediate batch. Validation is repeated
    /// at this seam so direct core callers cannot bypass the runtime contract.
    pub(crate) fn apply_runtime_options(
        &mut self,
        changes: HashMap<String, serde_json::Value>,
    ) -> Result<(), String> {
        for (key, value) in changes {
            if !self.try_update_option(&key, value)? {
                return Err(format!("Option '{}' cannot be changed at runtime", key));
            }
        }
        Ok(())
    }

    // ── Runtime Option Updates ──────────────────────────────────────────

    /// Update a single runtime-changeable option by key (using aria2's
    /// kebab-case option names, e.g. `"max-download-limit"`).
    ///
    /// Returns `true` if the option was recognized and updated, `false` if the
    /// key is not a runtime-changeable option. Invalid values are reported by
    /// [`Self::try_update_option`] and are intentionally not hidden here.
    ///
    /// For `max-download-limit` / `max-upload-limit`, the stored
    /// `RateLimiter` (if any) is also updated so the change takes effect
    /// immediately on the live download.
    pub fn validate_option_update(key: &str, value: &serde_json::Value) -> Result<bool, String> {
        let registry = crate::config::OptionRegistry::new();
        if registry.get(key).is_some() {
            registry
                .parse_rpc_value(key, value)
                .map_err(|error| format!("Option '{}': {}", key, error))?;
        }
        let mut options = super::DownloadOptions::default();
        apply_rpc_option(&mut options, key, value)
    }

    /// Apply a runtime option while preserving parse failures for RPC callers.
    pub fn try_update_option(
        &mut self,
        key: &str,
        value: serde_json::Value,
    ) -> Result<bool, String> {
        if !Self::validate_option_update(key, &value)? {
            return Ok(false);
        }

        let opts = Arc::make_mut(&mut self.options);
        let applied = apply_rpc_option(opts, key, &value)?;

        match key {
            "max-download-limit" => {
                if let Some(ref limiter) = *self.rate_limiter.recover() {
                    limiter.set_download_rate(opts.max_download_limit);
                }
            }
            "max-upload-limit" => {
                if let Some(ref limiter) = *self.rate_limiter.recover() {
                    limiter.set_upload_rate(opts.max_upload_limit);
                }
            }
            "split" => {
                tracing::warn!(
                    new_split = opts.split,
                    "split changed but will take effect on download restart/retry, \
                     not mid-download (current segments unchanged)"
                );
            }
            _ => {}
        }
        if applied {
            if let Ok(mut runtime_options) = self.runtime_options.write() {
                runtime_options.insert(key.to_string(), value);
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Compatibility wrapper for internal callers that only need to know
    /// whether a key is recognized. RPC-facing code should use
    /// [`Self::try_update_option`] so invalid values cannot be swallowed.
    pub fn update_option(&mut self, key: &str, value: serde_json::Value) -> bool {
        self.try_update_option(key, value).unwrap_or(false)
    }
}
