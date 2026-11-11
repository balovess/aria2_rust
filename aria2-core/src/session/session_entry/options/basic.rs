use super::DownloadOptions;
use std::collections::HashMap;

pub(super) fn apply(opts: &DownloadOptions, map: &mut HashMap<String, String>) {
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
    if let Some(v) = opts.max_http2_sessions_per_server {
        map.insert("max-http2-sessions-per-server".to_string(), v.to_string());
    }
    if let Some(v) = opts.max_http2_streams_per_session {
        map.insert("max-http2-streams-per-session".to_string(), v.to_string());
    }
    if opts.http_version != crate::http::HttpVersion::Auto {
        map.insert(
            "http-version".to_string(),
            opts.http_version.option_value().to_string(),
        );
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
    if !opts.enable_mmap {
        map.insert("enable-mmap".to_string(), "false".to_string());
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
}
