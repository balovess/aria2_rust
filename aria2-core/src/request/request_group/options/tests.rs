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
fn memory_follow_mode_is_limited_to_metadata_sources() {
    let options = DownloadOptions {
        follow_torrent: Some(FollowMode::Memory),
        follow_metalink: Some(FollowMode::Memory),
        ..DownloadOptions::default()
    };

    assert!(!options.uses_memory_download_for_uri("https://example.test/file.bin"));
    assert!(
        options.uses_memory_download_for_uri(
            "https://example.test/source.torrent?download=1#metadata"
        )
    );
    assert!(options.uses_memory_download_for_uri("https://example.test/index.meta4"));
    assert!(options.uses_memory_download_for_uri("/tmp/index.metalink3"));
}

#[test]
fn memory_follow_mode_matches_metadata_content_types() {
    let options = DownloadOptions {
        follow_torrent: Some(FollowMode::Memory),
        follow_metalink: Some(FollowMode::Memory),
        ..DownloadOptions::default()
    };

    assert!(
        options.uses_memory_download_for_content_type("application/x-bittorrent; charset=binary")
    );
    assert!(options.uses_memory_download_for_content_type("application/metalink4+xml"));
    assert!(!options.uses_memory_download_for_content_type("application/octet-stream"));
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

#[test]
fn proxy_credentials_fall_back_to_embedded_proxy_url_values() {
    let options = DownloadOptions {
        http_proxy: Some("http://url-user:url-pass@proxy.example:8080".to_string()),
        ..DownloadOptions::default()
    };

    assert_eq!(
        options.proxy_credentials_for_scheme("http"),
        (Some("url-user".to_string()), Some("url-pass".to_string()))
    );
}

#[test]
fn explicit_proxy_credentials_override_embedded_proxy_url_values() {
    let options = DownloadOptions {
        http_proxy: Some("http://url-user:url-pass@proxy.example:8080".to_string()),
        http_proxy_user: Some("option-user".to_string()),
        http_proxy_passwd: Some("option-pass".to_string()),
        ..DownloadOptions::default()
    };

    assert_eq!(
        options.proxy_credentials_for_scheme("http"),
        (
            Some("option-user".to_string()),
            Some("option-pass".to_string())
        )
    );
}

#[test]
fn embedded_proxy_credentials_follow_non_empty_proxy_fallback() {
    let options = DownloadOptions {
        http_proxy: Some(String::new()),
        all_proxy: Some("http://all-user:all-pass@proxy.example:8080".to_string()),
        ..DownloadOptions::default()
    };

    assert_eq!(
        options.proxy_credentials_for_scheme("http"),
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

#[cfg(feature = "bittorrent")]
#[test]
fn hash_check_controls_survive_option_conversion() {
    let values = HashMap::from([
        (
            "bt-enable-hook-after-hash-check".to_string(),
            "false".to_string(),
        ),
        ("bt-hash-check-seed".to_string(), "false".to_string()),
        ("bt-seed-unverified".to_string(), "true".to_string()),
        ("bt-remove-unselected-file".to_string(), "true".to_string()),
    ]);

    let options = DownloadOptions::from_option_strings(&values);

    assert!(!options.bt_enable_hook_after_hash_check);
    assert!(!options.bt_hash_check_seed);
    assert!(options.bt_seed_unverified);
    assert!(options.bt_remove_unselected_file);
}

#[cfg(feature = "bittorrent")]
#[test]
fn bt_seed_unverified_defaults_to_false() {
    assert!(!DownloadOptions::default().bt_seed_unverified);
    assert!(!DownloadOptions::from_option_strings(&HashMap::new()).bt_seed_unverified);
}
