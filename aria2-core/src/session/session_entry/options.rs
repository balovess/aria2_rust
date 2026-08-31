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
    if opts.force_sequential {
        map.insert("force-sequential".to_string(), "true".to_string());
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
    if let Some(v) = opts.disk_cache
        && v != crate::request::request_group::DEFAULT_DISK_CACHE_BYTES
    {
        map.insert("disk-cache".to_string(), v.to_string());
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
    if opts.allow_piece_length_change {
        map.insert("allow-piece-length-change".to_string(), "true".to_string());
    }
    map.insert("async-dns".to_string(), opts.async_dns.to_string());
    if let Some(v) = opts.mmap_threshold {
        map.insert("mmap-threshold".to_string(), v.to_string());
    }
    if opts.enable_mmap {
        map.insert("enable-mmap".to_string(), "true".to_string());
    }
    if let Some(v) = opts.max_mmap_limit {
        map.insert("max-mmap-limit".to_string(), v.to_string());
    }
    if let Some(v) = opts.no_file_allocation_limit
        && v != 5 * 1024 * 1024
    {
        map.insert("no-file-allocation-limit".to_string(), v.to_string());
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
    if !opts.bt_enable_hook_after_hash_check {
        map.insert(
            "bt-enable-hook-after-hash-check".to_string(),
            "false".to_string(),
        );
    }
    if !opts.bt_hash_check_seed {
        map.insert("bt-hash-check-seed".to_string(), "false".to_string());
    }
    if opts.bt_seed_unverified {
        map.insert("bt-seed-unverified".to_string(), "true".to_string());
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
    if let Some(ref v) = opts.metalink_base_uri {
        map.insert("metalink-base-uri".to_string(), v.clone());
    }
    if let Some(ref v) = opts.select_file {
        map.insert("select-file".to_string(), v.clone());
    }
    if opts.bt_remove_unselected_file {
        map.insert("bt-remove-unselected-file".to_string(), "true".to_string());
    }
    if !opts.metalink_enable_unique_protocol {
        map.insert(
            "metalink-enable-unique-protocol".to_string(),
            "false".to_string(),
        );
    }
    if let Some(v) = opts.min_split_size
        && v != crate::constants::DEFAULT_MIN_SPLIT_SIZE
    {
        map.insert("min-split-size".to_string(), v.to_string());
    }
    if opts.parameterized_uri {
        map.insert("parameterized-uri".to_string(), "true".to_string());
    }
    map.insert("reuse-uri".to_string(), opts.reuse_uri.to_string());
    if !opts.uri_selector.is_empty() {
        map.insert("uri-selector".to_string(), opts.uri_selector.clone());
    }
    if !opts.stream_piece_selector.is_empty() {
        map.insert(
            "stream-piece-selector".to_string(),
            opts.stream_piece_selector.clone(),
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
    if let Some(ref v) = opts.bt_exclude_tracker {
        map.insert("bt-exclude-tracker".to_string(), v.join(","));
    }
    if let Some(ref v) = opts.bt_external_ip {
        map.insert("bt-external-ip".to_string(), v.clone());
    }
    if opts.bt_load_saved_metadata {
        map.insert("bt-load-saved-metadata".to_string(), "true".to_string());
    }
    if opts.bt_metadata_only {
        map.insert("bt-metadata-only".to_string(), "true".to_string());
    }
    if opts.bt_min_crypto_level != "plain" {
        map.insert(
            "bt-min-crypto-level".to_string(),
            opts.bt_min_crypto_level.clone(),
        );
    }
    if opts.bt_request_peer_speed_limit != 50 * 1024 {
        map.insert(
            "bt-request-peer-speed-limit".to_string(),
            opts.bt_request_peer_speed_limit.to_string(),
        );
    }
    if opts.bt_save_metadata {
        map.insert("bt-save-metadata".to_string(), "true".to_string());
    }
    if !opts.bt_enable_web_seed {
        map.insert("bt-enable-web-seed".to_string(), "false".to_string());
    }
    if opts.bt_max_open_files != 100 {
        map.insert(
            "bt-max-open-files".to_string(),
            opts.bt_max_open_files.to_string(),
        );
    }
    if let Some(ref v) = opts.bt_peer_blocklist {
        map.insert("bt-peer-blocklist".to_string(), v.clone());
    }
    if opts.bt_keep_alive_interval != 120 {
        map.insert(
            "bt-keep-alive-interval".to_string(),
            opts.bt_keep_alive_interval.to_string(),
        );
    }
    if opts.bt_timeout != 180 {
        map.insert("bt-timeout".to_string(), opts.bt_timeout.to_string());
    }
    if opts.bt_request_timeout != 60 {
        map.insert(
            "bt-request-timeout".to_string(),
            opts.bt_request_timeout.to_string(),
        );
    }
    if opts.peer_connection_timeout != 20 {
        map.insert(
            "peer-connection-timeout".to_string(),
            opts.peer_connection_timeout.to_string(),
        );
    }
    if opts.peer_id_prefix != aria2_protocol::identity::DEFAULT_PEER_ID_PREFIX {
        map.insert("peer-id-prefix".to_string(), opts.peer_id_prefix.clone());
    }
    if opts.peer_agent != aria2_protocol::identity::DEFAULT_PEER_AGENT {
        map.insert("peer-agent".to_string(), opts.peer_agent.clone());
    }
    if opts.dht_message_timeout != 10 {
        map.insert(
            "dht-message-timeout".to_string(),
            opts.dht_message_timeout.to_string(),
        );
    }
    if opts.enable_dht6 {
        map.insert("enable-dht6".to_string(), "true".to_string());
    }
    if let Some(ref v) = opts.dht_listen_addr6 {
        map.insert("dht-listen-addr6".to_string(), v.clone());
    }
    if let Some(ref v) = opts.dht_entry_point_host {
        map.insert("dht-entry-point-host".to_string(), v.clone());
    }
    if let Some(v) = opts.dht_entry_point_port {
        map.insert("dht-entry-point-port".to_string(), v.to_string());
    }
    if let Some(ref v) = opts.dht_entry_point6 {
        map.insert("dht-entry-point6".to_string(), v.clone());
    }
    if let Some(ref v) = opts.dht_entry_point_host6 {
        map.insert("dht-entry-point-host6".to_string(), v.clone());
    }
    if let Some(v) = opts.dht_entry_point_port6 {
        map.insert("dht-entry-point-port6".to_string(), v.to_string());
    }
    if let Some(ref v) = opts.dht_file_path6 {
        map.insert("dht-file-path6".to_string(), v.clone());
    }
    if let Some(ref v) = opts.dht_listen_addr {
        map.insert("dht-listen-addr".to_string(), v.clone());
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
    if opts.bt_tracker_connect_timeout != 60 {
        map.insert(
            "bt-tracker-connect-timeout".to_string(),
            opts.bt_tracker_connect_timeout.to_string(),
        );
    }
    if opts.bt_tracker_interval != 0 {
        map.insert(
            "bt-tracker-interval".to_string(),
            opts.bt_tracker_interval.to_string(),
        );
    }
    if opts.bt_tracker_timeout != 60 {
        map.insert(
            "bt-tracker-timeout".to_string(),
            opts.bt_tracker_timeout.to_string(),
        );
    }
    if opts.bt_tracker_stopped_timeout != crate::constants::BT_TRACKER_STOPPED_TIMEOUT_SECS {
        map.insert(
            "bt-tracker-stopped-timeout".to_string(),
            opts.bt_tracker_stopped_timeout.to_string(),
        );
    }
    if !opts.enable_peer_exchange {
        map.insert("enable-peer-exchange".to_string(), "false".to_string());
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
    if let Some(ref v) = opts.http_proxy_user {
        map.insert("http-proxy-user".to_string(), v.clone());
    }
    if let Some(ref v) = opts.http_proxy_passwd {
        map.insert("http-proxy-passwd".to_string(), v.clone());
    }
    if let Some(ref v) = opts.all_proxy {
        map.insert("all-proxy".to_string(), v.clone());
    }
    if let Some(ref v) = opts.all_proxy_user {
        map.insert("all-proxy-user".to_string(), v.clone());
    }
    if let Some(ref v) = opts.all_proxy_passwd {
        map.insert("all-proxy-passwd".to_string(), v.clone());
    }
    if let Some(ref v) = opts.https_proxy {
        map.insert("https-proxy".to_string(), v.clone());
    }
    if let Some(ref v) = opts.https_proxy_user {
        map.insert("https-proxy-user".to_string(), v.clone());
    }
    if let Some(ref v) = opts.https_proxy_passwd {
        map.insert("https-proxy-passwd".to_string(), v.clone());
    }
    if let Some(ref v) = opts.ftp_proxy {
        map.insert("ftp-proxy".to_string(), v.clone());
    }
    if let Some(ref v) = opts.ftp_proxy_user {
        map.insert("ftp-proxy-user".to_string(), v.clone());
    }
    if let Some(ref v) = opts.ftp_proxy_passwd {
        map.insert("ftp-proxy-passwd".to_string(), v.clone());
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
    if !opts.enable_http_keep_alive {
        map.insert("enable-http-keep-alive".to_string(), "false".to_string());
    }
    if opts.enable_http_pipelining {
        map.insert("enable-http-pipelining".to_string(), "true".to_string());
    }
    if opts.http_accept_gzip {
        map.insert("http-accept-gzip".to_string(), "true".to_string());
    }
    if opts.http_no_cache {
        map.insert("http-no-cache".to_string(), "true".to_string());
    }
    if opts.use_head {
        map.insert("use-head".to_string(), "true".to_string());
    }
    if opts.no_want_digest_header {
        map.insert("no-want-digest-header".to_string(), "true".to_string());
    }
    if !opts.check_certificate {
        map.insert("check-certificate".to_string(), "false".to_string());
    }
    if let Some(ref v) = opts.ca_certificate {
        map.insert("ca-certificate".to_string(), v.clone());
    }
    if let Some(ref v) = opts.certificate {
        map.insert("certificate".to_string(), v.clone());
    }
    if let Some(ref v) = opts.private_key {
        map.insert("private-key".to_string(), v.clone());
    }
    if let Some(ref v) = opts.min_tls_version {
        map.insert("min-tls-version".to_string(), v.clone());
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
    if !opts.ftp_type.is_empty() {
        map.insert("ftp-type".to_string(), opts.ftp_type.clone());
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

    // --- Task lifecycle / HTTP response policy ---
    if opts.pause {
        map.insert("pause".to_string(), "true".to_string());
    }
    if opts.pause_metadata {
        map.insert("pause-metadata".to_string(), "true".to_string());
    }
    if opts.force_save {
        map.insert("force-save".to_string(), "true".to_string());
    }
    map.insert(
        "save-not-found".to_string(),
        opts.save_not_found.to_string(),
    );
    map.insert(
        "rpc-save-upload-metadata".to_string(),
        opts.rpc_save_upload_metadata.to_string(),
    );
    if opts.content_disposition_default_utf8 {
        map.insert(
            "content-disposition-default-utf8".to_string(),
            "true".to_string(),
        );
    }
    if !opts.proxy_method.is_empty() {
        map.insert("proxy-method".to_string(), opts.proxy_method.clone());
    }
    map.insert(
        "max-file-not-found".to_string(),
        opts.max_file_not_found.to_string(),
    );

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

/// Add the small set of initial options whose original wire spelling must be
/// preserved alongside typed execution options.
pub fn download_options_to_map_with_snapshot(
    opts: &DownloadOptions,
    snapshot: Option<&HashMap<String, serde_json::Value>>,
) -> HashMap<String, String> {
    let mut map = download_options_to_map(opts);
    let Some(snapshot) = snapshot else {
        return map;
    };

    let registry = crate::config::OptionRegistry::new();
    for name in crate::config::INITIAL_SNAPSHOT_WIRE_OPTIONS {
        let Some(value) = snapshot
            .get(*name)
            .and_then(crate::request::request_group::option_value_to_string)
        else {
            continue;
        };

        let is_default = registry.get(name).is_some_and(|definition| {
            definition
                .parse_value(&value)
                .ok()
                .is_some_and(|parsed| parsed.to_string() == definition.default_value().to_string())
        });
        if !is_default {
            map.insert((*name).to_string(), value);
        }
    }
    map
}
