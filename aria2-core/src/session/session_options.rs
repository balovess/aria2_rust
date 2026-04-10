//! DownloadOptions to session options conversion
//!
//! Provides utility functions to convert [`DownloadOptions`] struct
//! into key-value HashMap format suitable for session file serialization.
//!
//! # Mapped Fields
//!
//! | DownloadOptions Field | Session Key |
//! |---------------------|-------------|
//! | split | "split" |
//! | max_connection_per_server | "max-connection-per-server" |
//! | max_download_limit | "max-download-limit" |
//! | max_upload_limit | "max-upload-limit" |
//! | dir | "dir" |
//! | out | "out" |
//! | seed_time | "seed-time" |
//! | seed_ratio | "seed-ratio" |
//!
//! # Example
//!
//! ```rust
/// use aria2_core::session::session_options::download_options_to_map;
/// use aria2_core::request::request_group::DownloadOptions;
///
/// let opts = DownloadOptions {
///     split: Some(8),
///     max_connection_per_server: Some(4),
///     ..Default::default()
/// };
///
/// let map = download_options_to_map(&opts);
/// assert_eq!(map.get("split").unwrap(), "8");
/// assert_eq!(map.get("max-connection-per-server").unwrap(), "4");
/// ```

use std::collections::HashMap;

use crate::request::request_group::DownloadOptions;

/// Converts DownloadOptions struct to a HashMap for serialization
///
/// Extracts relevant fields from [`DownloadOptions`] and converts them
/// to key-value pairs suitable for session file format.
///
/// # Arguments
///
/// * `opts` - DownloadOptions struct from RequestGroup
///
/// # Returns
///
/// HashMap containing option key-value pairs (only non-None fields)
pub fn download_options_to_map(opts: &DownloadOptions) -> HashMap<String, String> {
    let mut map = HashMap::new();

    // Connection settings
    if let Some(v) = opts.split {
        map.insert("split".to_string(), v.to_string());
    }
    if let Some(v) = opts.max_connection_per_server {
        map.insert("max-connection-per-server".to_string(), v.to_string());
    }

    // Bandwidth limits
    if let Some(v) = opts.max_download_limit {
        map.insert("max-download-limit".to_string(), v.to_string());
    }
    if let Some(v) = opts.max_upload_limit {
        map.insert("max-upload-limit".to_string(), v.to_string());
    }

    // Output settings
    if let Some(ref v) = opts.dir {
        map.insert("dir".to_string(), v.clone());
    }
    if let Some(ref v) = opts.out {
        map.insert("out".to_string(), v.clone());
    }

    // Seeding settings (BitTorrent)
    if let Some(v) = opts.seed_time {
        map.insert("seed-time".to_string(), v.to_string());
    }
    if let Some(v) = opts.seed_ratio {
        map.insert("seed-ratio".to_string(), v.to_string());
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_download_options_to_map_coverage() {
        let opts = DownloadOptions {
            split: Some(8),
            max_connection_per_server: Some(4),
            max_download_limit: Some(102400),
            max_upload_limit: Some(51200),
            dir: Some("/data".to_string()),
            out: Some("output.bin".to_string()),
            seed_time: Some(300),
            seed_ratio: Some(2.0),
            checksum: None,
            cookie_file: None,
            cookies: None,
            bt_force_encrypt: false,
            bt_require_crypto: false,
            enable_dht: true,
            dht_listen_port: Some(6881),
            enable_public_trackers: true,
            bt_piece_selection_strategy: "rarest-first".to_string(),
            bt_endgame_threshold: 20,
            max_retries: 3,
            retry_wait: 1,
            http_proxy: None,
            dht_file_path: None,
            bt_max_upload_slots: Some(4),
            bt_optimistic_unchoke_interval: Some(30),
            bt_snubbed_timeout: Some(60),
        };

        let map = download_options_to_map(&opts);
        assert_eq!(map.get("split").unwrap(), "8");
        assert_eq!(map.get("seed-ratio").unwrap(), "2");

        let empty_opts = DownloadOptions::default();
        let empty_map = download_options_to_map(&empty_opts);
        assert!(empty_map.is_empty());
    }
}
