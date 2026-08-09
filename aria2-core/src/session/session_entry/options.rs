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
    if opts.continue_download {
        map.insert("continue".to_string(), "true".to_string());
    }
    if opts.allow_overwrite {
        map.insert("allow-overwrite".to_string(), "true".to_string());
    }
    if !opts.auto_file_renaming {
        map.insert("auto-file-renaming".to_string(), "false".to_string());
    }
    if !opts.always_resume {
        map.insert("always-resume".to_string(), "false".to_string());
    }
    if opts.max_resume_failure_tries > 0 {
        map.insert(
            "max-resume-failure-tries".to_string(),
            opts.max_resume_failure_tries.to_string(),
        );
    }
    if opts.remove_control_file {
        map.insert("remove-control-file".to_string(), "true".to_string());
    }
    if let Some(v) = opts.seed_time {
        map.insert("seed-time".to_string(), v.to_string());
    }
    if let Some(v) = opts.seed_ratio
        && (v - 1.0).abs() > f64::EPSILON
    {
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

    // --- Integrity / Metalink selection ---
    if opts.check_integrity {
        map.insert("check-integrity".to_string(), "true".to_string());
    }
    if opts.hash_check_only {
        map.insert("hash-check-only".to_string(), "true".to_string());
    }
    if let Some(ref v) = opts.metalink_version {
        map.insert("metalink-version".to_string(), v.clone());
    }
    if let Some(ref v) = opts.metalink_language {
        map.insert("metalink-language".to_string(), v.clone());
    }
    if let Some(ref v) = opts.metalink_os {
        map.insert("metalink-os".to_string(), v.clone());
    }
    if let Some(ref v) = opts.metalink_location {
        map.insert("metalink-location".to_string(), v.clone());
    }
    if let Some(ref v) = opts.metalink_preferred_protocol {
        map.insert("metalink-preferred-protocol".to_string(), v.clone());
    }
    if let Some(ref v) = opts.select_file {
        map.insert("select-file".to_string(), v.clone());
    }
    if !opts.metalink_enable_unique_protocol {
        map.insert(
            "metalink-enable-unique-protocol".to_string(),
            "false".to_string(),
        );
    }

    // --- Checksum ---
    if let Some((ref algo, ref val)) = opts.checksum {
        map.insert("checksum".to_string(), format!("{}={}", algo, val));
    }

    // --- Cookies ---
    if let Some(ref v) = opts.cookie_file {
        map.insert("load-cookies".to_string(), v.clone());
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
    if opts.bt_max_peers != 55 {
        map.insert("bt-max-peers".to_string(), opts.bt_max_peers.to_string());
    }
    // enable_dht defaults to true; only save if disabled
    if !opts.enable_dht {
        map.insert("enable-dht".to_string(), "false".to_string());
    }
    if let Some(ref v) = opts.dht_listen_port {
        map.insert("dht-listen-port".to_string(), v.clone());
    }
    if let Some(ref v) = opts.listen_port {
        map.insert("listen-port".to_string(), v.clone());
    }
    if let Some(ref v) = opts.index_out {
        map.insert("index-out".to_string(), v.clone());
    }
    if let Some(ref v) = opts.dht_entry_point {
        map.insert("dht-entry-point".to_string(), v.join(","));
    }
    if let Some(ref v) = opts.bt_tracker {
        map.insert("bt-tracker".to_string(), v.join(","));
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

    // --- Piece sizing and connection behaviour ---
    if let Some(v) = opts.piece_length {
        map.insert("piece-length".to_string(), v.to_string());
    }
    if let Some(v) = opts.timeout {
        map.insert("timeout".to_string(), v.to_string());
    }
    if let Some(v) = opts.connect_timeout {
        map.insert("connect-timeout".to_string(), v.to_string());
    }
    if let Some(v) = opts.startup_idle_time {
        map.insert("startup-idle-time".to_string(), v.to_string());
    }
    if let Some(v) = opts.lowest_speed_limit {
        map.insert("lowest-speed-limit".to_string(), v.to_string());
    }
    if !opts.ftp_pasv {
        map.insert("ftp-pasv".to_string(), "false".to_string());
    }
    if opts.remote_time {
        map.insert("remote-time".to_string(), "true".to_string());
    }
    if opts.dry_run {
        map.insert("dry-run".to_string(), "true".to_string());
    }
    if !opts.ftp_reuse_connection {
        map.insert("ftp-reuse-connection".to_string(), "false".to_string());
    }
    if !opts.realtime_chunk_checksum {
        map.insert("realtime-chunk-checksum".to_string(), "false".to_string());
    }
    if let Some(v) = opts.bt_stop_timeout {
        map.insert("bt-stop-timeout".to_string(), v.to_string());
    }
    if opts.disable_ipv6 {
        map.insert("disable-ipv6".to_string(), "true".to_string());
    }
    if opts.bt_enable_lpd {
        map.insert("bt-enable-lpd".to_string(), "true".to_string());
    }
    if let Some(ref v) = opts.bt_lpd_interface {
        map.insert("bt-lpd-interface".to_string(), v.clone());
    }

    // --- Authentication and netrc ---
    if opts.http_auth_challenge {
        map.insert("http-auth-challenge".to_string(), "true".to_string());
    }
    if let Some(ref v) = opts.http_user {
        map.insert("http-user".to_string(), v.clone());
    }
    if let Some(ref v) = opts.http_passwd {
        map.insert("http-passwd".to_string(), v.clone());
    }
    if let Some(ref v) = opts.ftp_user {
        map.insert("ftp-user".to_string(), v.clone());
    }
    if let Some(ref v) = opts.ftp_passwd {
        map.insert("ftp-passwd".to_string(), v.clone());
    }
    if opts.no_netrc {
        map.insert("no-netrc".to_string(), "true".to_string());
    }
    if let Some(ref v) = opts.netrc_path {
        map.insert("netrc-path".to_string(), v.clone());
    }
    if opts.conditional_get {
        map.insert("conditional-get".to_string(), "true".to_string());
    }

    // --- Metadata follow modes ---
    if let Some(mode) = opts.follow_torrent {
        map.insert("follow-torrent".to_string(), mode.as_str().to_string());
    }
    if let Some(mode) = opts.follow_metalink {
        map.insert("follow-metalink".to_string(), mode.as_str().to_string());
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
