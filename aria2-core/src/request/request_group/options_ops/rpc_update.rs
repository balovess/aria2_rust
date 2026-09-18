//! Runtime option updates and rate limiter management.
//!
//! Implements `RequestGroup::update_option()` for dynamically changing
//! download options at runtime (e.g. via `aria2.changeOption`), and
//! the `set_rate_limiter` / `set_download_context` methods.

use std::collections::HashMap;
fn rpc_option_string(value: &serde_json::Value, key: &str) -> Result<String, String> {
    super::super::options::option_value_to_string(value)
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

pub(super) fn apply_rpc_option(
    opts: &mut super::super::DownloadOptions,
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
        | "bt-peer-blocklist"
        | "peer-id-prefix"
        | "peer-agent"
        | "dht-listen-addr6"
        | "dht-entry-point-host"
        | "dht-entry-point6"
        | "dht-entry-point-host6"
        | "dht-file-path6"
        | "dht-listen-addr"
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
            let mode = super::super::FollowMode::parse(&raw)
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
