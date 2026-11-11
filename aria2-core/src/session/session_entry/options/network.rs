use super::DownloadOptions;
use std::collections::HashMap;

pub(super) fn apply(opts: &DownloadOptions, map: &mut HashMap<String, String>) {
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
    if let Some(v) = opts.sftp_connect_timeout {
        map.insert("sftp-connect-timeout".to_string(), v.to_string());
    }
    if let Some(v) = opts.sftp_read_timeout {
        map.insert("sftp-read-timeout".to_string(), v.to_string());
    }
    if opts.sftp_compression {
        map.insert("sftp-compression".to_string(), "true".to_string());
    }
    if let Some(v) = opts.sftp_keepalive_interval {
        map.insert("sftp-keepalive-interval".to_string(), v.to_string());
    }
    if let Some(ref ciphers) = opts.sftp_preferred_ciphers {
        map.insert("sftp-preferred-ciphers".to_string(), ciphers.join(","));
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
    if !opts.enable_http_pipelining {
        map.insert("enable-http-pipelining".to_string(), "false".to_string());
    }
    if opts.http_accept_gzip {
        map.insert("http-accept-gzip".to_string(), "true".to_string());
    }
    if opts.http_no_cache {
        map.insert("http-no-cache".to_string(), "true".to_string());
    }
    if !opts.use_head {
        map.insert("use-head".to_string(), "false".to_string());
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
}
