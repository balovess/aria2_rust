use super::DownloadOptions;
use std::collections::HashMap;

pub(super) fn apply(opts: &DownloadOptions, map: &mut HashMap<String, String>) {
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
    if let Some(v) = opts.min_http_range_size
        && v != crate::constants::DEFAULT_HTTP_RANGE_SIZE_FLOOR_BYTES
    {
        map.insert("min-http-range-size".to_string(), v.to_string());
    }
    let adaptive = opts.http_adaptive;
    let adaptive_defaults = crate::request::request_group::HttpAdaptiveOptions::default();
    if adaptive.enabled != adaptive_defaults.enabled {
        map.insert(
            "http-adaptive-enabled".to_string(),
            adaptive.enabled.to_string(),
        );
    }
    for (name, value, default) in [
        (
            "http-adaptive-reduction-percent",
            adaptive.reduction_percent as u64,
            adaptive_defaults.reduction_percent as u64,
        ),
        (
            "http-adaptive-backoff-base",
            adaptive.backoff_base_secs,
            adaptive_defaults.backoff_base_secs,
        ),
        (
            "http-adaptive-backoff-max",
            adaptive.backoff_max_secs,
            adaptive_defaults.backoff_max_secs,
        ),
    ] {
        if value != default {
            map.insert(name.to_string(), value.to_string());
        }
    }
    if !opts.parameterized_uri {
        map.insert("parameterized-uri".to_string(), "false".to_string());
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
}
