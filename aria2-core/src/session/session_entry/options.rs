//! Download options conversion for session serialization
//!
//! Converts [`DownloadOptions`] struct to a `HashMap<String, String>` for
//! compact session file storage. Only non-default / non-empty values are
//! included; the load path uses `unwrap_or(default)` for every field, so
//! absent keys are safe.

use std::collections::HashMap;

use crate::request::request_group::DownloadOptions;

/// Converts DownloadOptions struct to a HashMap for serialization.
///
/// Only non-default / non-empty values are included to keep the session file
/// compact. The load path (`map_entry_to_download_options`) uses
/// `unwrap_or(default)` for every field, so absent keys are safe.
pub fn download_options_to_map(opts: &DownloadOptions) -> HashMap<String, String> {
    let mut map = HashMap::new();

    // --- Basic download options ---
    if let Some(v) = opts.split {
        map.insert("split".to_string(), v.to_string());
    }
    if let Some(v) = opts.max_connection_per_server {
        map.insert("max-connection-per-server".to_string(), v.to_string());
    }
    if let Some(v) = opts.max_download_limit {
        map.insert("max-download-limit".to_string(), v.to_string());
    }
    if let Some(v) = opts.max_upload_limit {
        map.insert("max-upload-limit".to_string(), v.to_string());
    }
    if let Some(ref v) = opts.dir {
        map.insert("dir".to_string(), v.clone());
    }
    if let Some(ref v) = opts.out {
        map.insert("out".to_string(), v.clone());
    }
    if let Some(v) = opts.seed_time {
        map.insert("seed-time".to_string(), v.to_string());
    }
    if let Some(v) = opts.seed_ratio {
        map.insert("seed-ratio".to_string(), v.to_string());
    }

    // --- File allocation ---
    if let Some(ref v) = opts.file_allocation {
        map.insert("file-allocation".to_string(), v.clone());
    }
    if let Some(v) = opts.mmap_threshold {
        map.insert("mmap-threshold".to_string(), v.to_string());
    }
    if opts.secure_falloc {
        map.insert("secure-falloc".to_string(), "true".to_string());
    }

    // --- Checksum ---
    if let Some((ref algo, ref val)) = opts.checksum {
        map.insert("checksum".to_string(), format!("{}={}", algo, val));
    }

    // --- Cookies ---
    if let Some(ref v) = opts.cookie_file {
        map.insert("cookie-file".to_string(), v.clone());
    }
    if let Some(ref v) = opts.cookies {
        map.insert("cookies".to_string(), v.clone());
    }

    // --- BitTorrent options ---
    if opts.bt_force_encrypt {
        map.insert("bt-force-encrypt".to_string(), "true".to_string());
    }
    if opts.bt_require_crypto {
        map.insert("bt-require-crypto".to_string(), "true".to_string());
    }
    // enable_dht defaults to true; only save if disabled
    if !opts.enable_dht {
        map.insert("enable-dht".to_string(), "false".to_string());
    }
    if let Some(v) = opts.dht_listen_port {
        map.insert("dht-listen-port".to_string(), v.to_string());
    }
    if let Some(ref v) = opts.dht_entry_point {
        map.insert("dht-entry-point".to_string(), v.join(","));
    }
    // enable_public_trackers defaults to true; only save if disabled
    if !opts.enable_public_trackers {
        map.insert("enable-public-trackers".to_string(), "false".to_string());
    }
    if !opts.bt_piece_selection_strategy.is_empty() {
        map.insert(
            "bt-piece-selection-strategy".to_string(),
            opts.bt_piece_selection_strategy.clone(),
        );
    }
    if opts.bt_endgame_threshold > 0 {
        map.insert(
            "bt-endgame-threshold".to_string(),
            opts.bt_endgame_threshold.to_string(),
        );
    }
    if let Some(v) = opts.bt_max_upload_slots {
        map.insert("bt-max-upload-slots".to_string(), v.to_string());
    }
    if let Some(v) = opts.bt_optimistic_unchoke_interval {
        map.insert("bt-optimistic-unchoke-interval".to_string(), v.to_string());
    }
    if let Some(v) = opts.bt_snubbed_timeout {
        map.insert("bt-snubbed-timeout".to_string(), v.to_string());
    }
    if !opts.bt_prioritize_piece.is_empty() {
        map.insert(
            "bt-prioritize-piece".to_string(),
            opts.bt_prioritize_piece.clone(),
        );
    }
    if opts.bt_detach_seed_only {
        map.insert("bt-detach-seed-only".to_string(), "true".to_string());
    }
    if opts.enable_utp {
        map.insert("enable-utp".to_string(), "true".to_string());
    }
    if let Some(v) = opts.utp_listen_port {
        map.insert("utp-listen-port".to_string(), v.to_string());
    }

    // --- Retry options ---
    if opts.max_retries > 0 {
        map.insert("max-retries".to_string(), opts.max_retries.to_string());
    }
    if opts.retry_wait > 0 {
        map.insert("retry-wait".to_string(), opts.retry_wait.to_string());
    }

    // --- DHT file path ---
    if let Some(ref v) = opts.dht_file_path {
        map.insert("dht-file-path".to_string(), v.clone());
    }

    // --- Proxy options ---
    if let Some(ref v) = opts.http_proxy {
        map.insert("http-proxy".to_string(), v.clone());
    }
    if let Some(ref v) = opts.all_proxy {
        map.insert("all-proxy".to_string(), v.clone());
    }
    if let Some(ref v) = opts.https_proxy {
        map.insert("https-proxy".to_string(), v.clone());
    }
    if let Some(ref v) = opts.ftp_proxy {
        map.insert("ftp-proxy".to_string(), v.clone());
    }
    if let Some(ref v) = opts.no_proxy {
        map.insert("no-proxy".to_string(), v.clone());
    }

    // --- SSH / SFTP ---
    if let Some(ref v) = opts.ssh_host_key_md {
        map.insert("ssh-host-key-md".to_string(), v.clone());
    }

    // --- HTTP headers ---
    if !opts.header.is_empty() {
        map.insert("header".to_string(), opts.header.join(","));
    }
    if let Some(ref v) = opts.user_agent {
        map.insert("user-agent".to_string(), v.clone());
    }
    if let Some(ref v) = opts.referer {
        map.insert("referer".to_string(), v.clone());
    }

    // --- Event hooks ---
    if let Some(ref v) = opts.on_download_start {
        map.insert("on-download-start".to_string(), v.clone());
    }
    if let Some(ref v) = opts.on_download_complete {
        map.insert("on-download-complete".to_string(), v.clone());
    }
    if let Some(ref v) = opts.on_download_error {
        map.insert("on-download-error".to_string(), v.clone());
    }
    if let Some(ref v) = opts.on_download_pause {
        map.insert("on-download-pause".to_string(), v.clone());
    }
    if let Some(ref v) = opts.on_download_stop {
        map.insert("on-download-stop".to_string(), v.clone());
    }
    if let Some(ref v) = opts.on_bt_download_complete {
        map.insert("on-bt-download-complete".to_string(), v.clone());
    }

    map
}
