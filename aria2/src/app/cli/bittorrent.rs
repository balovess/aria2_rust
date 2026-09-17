use std::path::PathBuf;

use clap::Args;

// BitTorrent Options
// =========================================================================

/// BitTorrent options: seeding, DHT, PEX, peer management.
#[derive(Args, Debug)]
#[command(next_help_heading = "BitTorrent options")]
pub struct BitTorrentArgs {
    /// Seeding time in minutes (0=infinite)
    #[arg(short = 'G', long = "seed-time")]
    pub seed_time: Option<f64>,

    /// Share ratio threshold
    #[arg(short = 'g', long = "seed-ratio")]
    pub seed_ratio: Option<f64>,

    /// Max peers per torrent
    #[arg(short = 'B', long = "bt-max-peers")]
    pub bt_max_peers: Option<u64>,

    /// Min peer speed to stay connected
    #[arg(long = "bt-request-peer-speed-limit")]
    pub bt_request_peer_speed_limit: Option<String>,

    /// Max open files for BT
    #[arg(long = "bt-max-open-files")]
    pub bt_max_open_files: Option<u64>,

    /// Path to the BitTorrent peer blocklist
    #[arg(long = "bt-peer-blocklist", hide = true)]
    pub bt_peer_blocklist: Option<PathBuf>,

    /// BitTorrent peer keep-alive interval in seconds
    #[arg(long = "bt-keep-alive-interval", hide = true)]
    pub bt_keep_alive_interval: Option<u64>,

    /// BitTorrent peer inactivity timeout in seconds
    #[arg(long = "bt-timeout", hide = true)]
    pub bt_timeout: Option<u64>,

    /// BitTorrent piece request timeout in seconds
    #[arg(long = "bt-request-timeout", hide = true)]
    pub bt_request_timeout: Option<u64>,

    /// BitTorrent peer connection timeout in seconds
    #[arg(long = "peer-connection-timeout", hide = true)]
    pub peer_connection_timeout: Option<u64>,

    /// Seed without verifying hash
    #[arg(
        long = "bt-seed-unverified",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_seed_unverified: Option<bool>,

    /// Save metadata as .torrent file
    #[arg(
        long = "bt-save-metadata",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_save_metadata: Option<bool>,

    /// Force BT encryption
    #[arg(
        short = 'X',
        long = "bt-force-encryption",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_force_encryption: Option<bool>,

    /// Min crypto level (plain/arc4)
    #[arg(long = "bt-min-crypto-level")]
    pub bt_min_crypto_level: Option<String>,

    /// Enable Local Peer Discovery
    #[arg(
        long = "bt-enable-lpd",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_enable_lpd: Option<bool>,

    /// Enable Local Peer Discovery (alias)
    #[arg(
        long = "enable-lpd",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_lpd: Option<bool>,

    /// UDP port for Local Peer Discovery
    #[arg(long = "lpd-listen-port")]
    pub lpd_listen_port: Option<u64>,

    /// Enable web seed (HTTP/FTP seeding)
    #[arg(
        long = "bt-enable-web-seed",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_enable_web_seed: Option<bool>,

    /// Enable DHT
    #[arg(
        long = "enable-dht",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_dht: Option<bool>,

    /// Disable DHT
    #[arg(
        long = "no-enable-dht",
        hide = true,
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub no_enable_dht: Option<bool>,

    /// DHT listen port
    #[arg(long = "dht-listen-port")]
    pub dht_listen_port: Option<String>,

    /// IPv4 address for DHT to listen on
    #[arg(long = "dht-listen-addr", hide = true)]
    pub dht_listen_addr: Option<String>,

    /// DHT bootstrap nodes (host:port format, comma-separated)
    #[arg(long = "dht-entry-point")]
    pub dht_entry_point: Option<String>,

    /// IPv4 DHT bootstrap node hostname
    #[arg(long = "dht-entry-point-host", hide = true)]
    pub dht_entry_point_host: Option<String>,

    /// IPv4 DHT bootstrap node port
    #[arg(long = "dht-entry-point-port", hide = true)]
    pub dht_entry_point_port: Option<u16>,

    /// Path to DHT routing table file for persistence
    #[arg(long = "dht-file-path")]
    pub dht_file_path: Option<PathBuf>,

    /// DHT message cache path (deprecated)
    #[arg(long = "dht-message-path")]
    pub dht_message_path: Option<PathBuf>,

    /// Enable PEX
    #[arg(
        long = "enable-peer-exchange",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_peer_exchange: Option<bool>,

    /// Auto-handle .torrent (true/false/mem)
    #[arg(long = "follow-torrent")]
    pub follow_torrent: Option<String>,

    /// Command on BT download complete
    #[arg(long = "on-bt-download-complete")]
    pub on_bt_download_complete: Option<String>,

    /// Command on BT download error
    #[arg(long = "on-bt-download-error")]
    pub on_bt_download_error: Option<String>,

    /// Listening port range (e.g. 6881-6999)
    #[arg(short = 'L', long = "listen-port")]
    pub listen_port: Option<String>,

    /// Piece selection priority mode (rarest/head/tail)
    #[arg(long = "bt-prioritize-piece")]
    pub bt_prioritize_piece: Option<String>,

    /// Enable uTP (UDP Transport Protocol, BEP 29). Experimental
    #[arg(
        long = "enable-utp",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_utp: Option<bool>,

    /// UDP port for uTP connections. 0 = auto-assign
    #[arg(long = "utp-listen-port")]
    pub utp_listen_port: Option<u64>,

    /// Detach seed-only downloads from main session
    #[arg(
        long = "bt-detach-seed-only",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_detach_seed_only: Option<bool>,

    /// Run hook after hash check
    #[arg(
        long = "bt-enable-hook-after-hash-check",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_enable_hook_after_hash_check: Option<bool>,

    /// Comma-separated list of tracker announce URIs to exclude
    #[arg(long = "bt-exclude-tracker")]
    pub bt_exclude_tracker: Option<String>,

    /// External IP address for BitTorrent
    #[arg(long = "bt-external-ip")]
    pub bt_external_ip: Option<String>,

    /// Seed after hash check
    #[arg(
        long = "bt-hash-check-seed",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_hash_check_seed: Option<bool>,

    /// Load saved metadata from previous session
    #[arg(
        long = "bt-load-saved-metadata",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_load_saved_metadata: Option<bool>,

    /// Network interface for Local Peer Discovery
    #[arg(long = "bt-lpd-interface")]
    pub bt_lpd_interface: Option<String>,

    /// Download only torrent metadata
    #[arg(
        long = "bt-metadata-only",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_metadata_only: Option<bool>,

    /// Remove unselected files when --select-file is used
    #[arg(
        long = "bt-remove-unselected-file",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_remove_unselected_file: Option<bool>,

    /// Require BitTorrent message encryption
    #[arg(
        long = "bt-require-crypto",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub bt_require_crypto: Option<bool>,

    /// Stop BT download after N seconds without progress
    #[arg(long = "bt-stop-timeout")]
    pub bt_stop_timeout: Option<u64>,

    /// Comma-separated list of tracker announce URIs
    #[arg(long = "bt-tracker")]
    pub bt_tracker: Option<String>,

    /// Enable public trackers
    #[arg(
        long = "enable-public-trackers",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_public_trackers: Option<bool>,

    /// Remote public tracker list sources, comma or newline separated
    #[arg(long = "bt-tracker-source")]
    pub bt_tracker_source: Option<String>,

    /// Public tracker list refresh interval in seconds
    #[arg(long = "bt-tracker-update-interval")]
    pub bt_tracker_update_interval: Option<u64>,

    /// Connect timeout for tracker in seconds
    #[arg(long = "bt-tracker-connect-timeout")]
    pub bt_tracker_connect_timeout: Option<u64>,

    /// Tracker announce interval in seconds
    #[arg(long = "bt-tracker-interval")]
    pub bt_tracker_interval: Option<u64>,

    /// Timeout for tracker in seconds
    #[arg(long = "bt-tracker-timeout")]
    pub bt_tracker_timeout: Option<u64>,

    /// Total timeout for stopped tracker announces in seconds
    #[arg(long = "bt-tracker-stopped-timeout")]
    pub bt_tracker_stopped_timeout: Option<u64>,

    /// DHT message timeout in seconds
    #[arg(long = "dht-message-timeout")]
    pub dht_message_timeout: Option<u64>,

    /// Enable IPv6 DHT
    #[arg(
        long = "enable-dht6",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_dht6: Option<bool>,

    /// IPv6 address for DHT to listen on
    #[arg(long = "dht-listen-addr6")]
    pub dht_listen_addr6: Option<String>,

    /// IPv6 DHT bootstrap node (hostname:port)
    #[arg(long = "dht-entry-point6")]
    pub dht_entry_point6: Option<String>,

    /// IPv6 DHT bootstrap node hostname
    #[arg(long = "dht-entry-point-host6", hide = true)]
    pub dht_entry_point_host6: Option<String>,

    /// IPv6 DHT bootstrap node port
    #[arg(long = "dht-entry-point-port6", hide = true)]
    pub dht_entry_point_port6: Option<u16>,

    /// Path to IPv6 DHT routing table file
    #[arg(long = "dht-file-path6")]
    pub dht_file_path6: Option<PathBuf>,

    /// Peer ID prefix for BitTorrent
    #[arg(long = "peer-id-prefix")]
    pub peer_id_prefix: Option<String>,

    /// Peer agent string for BitTorrent
    #[arg(long = "peer-agent")]
    pub peer_agent: Option<String>,

    /// Comma-separated list of file indices to download (BT/Metalink, 1-indexed)
    #[arg(long = "select-file")]
    pub select_file: Option<String>,

    /// Set output filename for a BitTorrent file index (INDEX=PATH, repeatable)
    #[arg(short = 'O', long = "index-out")]
    pub index_out: Vec<String>,
}
