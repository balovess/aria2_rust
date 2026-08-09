//! BitTorrent category options: seeding, DHT, PEX, peers, encryption, uTP.

use crate::config::{OptionCategory, OptionDef, OptionType, OptionValue};

impl crate::config::OptionRegistry {
    /// Register BitTorrent-specific options: seeding, DHT, PEX, peer management.
    pub fn register_bt_options(&mut self) {
        // --- Seeding Settings ---
        self.register(OptionDef {
            name: "seed-time".into(),
            opt_type: OptionType::Float,
            short_name: Some('G'),
            default_value: OptionValue::Float(0.0),
            description: "Seeding time in minutes (0=infinite)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "seed-ratio".into(),
            opt_type: OptionType::Float,
            short_name: Some('g'),
            default_value: OptionValue::Float(1.0),
            description: "Share ratio threshold".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- Peer Management ---
        self.register(OptionDef {
            name: "bt-max-peers".into(),
            opt_type: OptionType::Integer,
            short_name: Some('B'),
            default_value: OptionValue::Int(55),
            min: Some(0),
            description: "Max peers per torrent".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-request-peer-speed-limit".into(),
            opt_type: OptionType::Size,
            default_value: OptionValue::Int((50 * 1024) as i64),
            description: "Min peer speed to stay connected".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-max-open-files".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(100),
            min: Some(1),
            description: "Max open files for BT".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        // --- Torrent Behavior ---
        self.register(OptionDef {
            name: "bt-seed-unverified".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Seed without verifying hash".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-save-metadata".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Save metadata as .torrent file".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- Encryption ---
        self.register(OptionDef {
            name: "bt-force-encryption".into(),
            opt_type: OptionType::Boolean,
            short_name: Some('X'),
            default_value: OptionValue::Bool(false),
            description: "Force BT encryption".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-min-crypto-level".into(),
            opt_type: OptionType::Enum,
            default_value: OptionValue::Str("plain".into()),
            allowed_values: &["plain", "arc4"],
            description: "Min crypto level (plain/arc4)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- DHT / LPD / PEX ---
        self.register(OptionDef {
            name: "bt-enable-lpd".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Enable Local Peer Discovery".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "enable-lpd".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Enable Local Peer Discovery (alias for bt-enable-lpd)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "lpd-listen-port".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(6771),
            min: Some(1024),
            max: Some(65535),
            description: "UDP port for Local Peer Discovery".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-enable-web-seed".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Enable web seed (HTTP/FTP seeding)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "enable-dht".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Enable DHT".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dht-listen-port".into(),
            opt_type: OptionType::IntegerRange,
            default_value: OptionValue::Str("6881-6999".into()),
            min: Some(1024),
            max: Some(65535),
            description: "DHT listen port".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dht-entry-point".into(),
            opt_type: OptionType::List,
            description: "DHT bootstrap nodes (host:port format, comma-separated)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dht-file-path".into(),
            opt_type: OptionType::Path,
            description: "Path to DHT routing table file for persistence".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dht-message-path".into(),
            opt_type: OptionType::Path,
            description: "DHT message cache path (deprecated, use dht-file-path instead)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "enable-peer-exchange".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Enable PEX".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- Torrent Handling ---
        self.register(OptionDef {
            name: "follow-torrent".into(),
            opt_type: OptionType::Enum,
            default_value: OptionValue::Str("true".into()),
            allowed_values: &["true", "false", "mem"],
            description: "Auto-handle .torrent (true/false/mem)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- Event Hooks ---
        self.register(OptionDef {
            name: "on-bt-download-complete".into(),
            opt_type: OptionType::String,
            description: "Command on BT download complete".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "on-bt-download-error".into(),
            opt_type: OptionType::String,
            description: "Command on BT download error".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- Listening Port ---
        self.register(OptionDef {
            name: "listen-port".into(),
            opt_type: OptionType::IntegerRange,
            default_value: OptionValue::Str("6881-6999".into()),
            min: Some(1024),
            max: Some(65535),
            description: "Listening port range".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- Piece Selection Priority (G2) ---
        self.register(OptionDef {
            name: "bt-prioritize-piece".into(),
            opt_type: OptionType::String,
            default_value: OptionValue::Str("rarest".into()),
            description: "Piece selection priority mode: 'rarest' (default), 'head' (sequential from start), 'tail' (sequential from end)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- BT Additional Options ---
        self.register(OptionDef {
            name: "bt-detach-seed-only".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Detach seed-only downloads from main session".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-enable-hook-after-hash-check".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Run hook after hash check (--on-bt-download-complete/error)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-exclude-tracker".into(),
            opt_type: OptionType::List,
            description: "Comma-separated list of tracker announce URIs to exclude".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-external-ip".into(),
            opt_type: OptionType::String,
            description: "External IP address for BitTorrent".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-hash-check-seed".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Seed after hash check when --check-integrity is used".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-load-saved-metadata".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Load saved metadata from previous session".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-lpd-interface".into(),
            opt_type: OptionType::String,
            description: "Network interface for Local Peer Discovery".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-metadata-only".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Download only torrent metadata".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-remove-unselected-file".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Remove unselected files when --select-file is used".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-require-crypto".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Require BitTorrent message encryption".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-stop-timeout".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(0),
            min: Some(0),
            description: "Stop BT download after N seconds without progress (0=disabled)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-tracker".into(),
            opt_type: OptionType::List,
            description: "Comma-separated list of tracker announce URIs".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-tracker-connect-timeout".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(60),
            min: Some(1),
            max: Some(600),
            description: "Connect timeout for tracker in seconds".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-tracker-interval".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(0),
            min: Some(0),
            description: "Tracker announce interval in seconds (0=use tracker's value)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-tracker-timeout".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(60),
            min: Some(1),
            max: Some(600),
            description: "Timeout for tracker in seconds".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dht-message-timeout".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(10),
            min: Some(1),
            max: Some(60),
            description: "DHT message timeout in seconds".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "enable-dht6".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Enable IPv6 DHT".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dht-listen-addr6".into(),
            opt_type: OptionType::String,
            description: "IPv6 address for DHT to listen on".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "peer-id-prefix".into(),
            opt_type: OptionType::String,
            default_value: OptionValue::Str(
                aria2_protocol::identity::DEFAULT_PEER_ID_PREFIX.into(),
            ),
            description: "Peer ID prefix for BitTorrent".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "peer-agent".into(),
            opt_type: OptionType::String,
            default_value: OptionValue::Str(aria2_protocol::identity::DEFAULT_PEER_AGENT.into()),
            description: "Peer agent string for BitTorrent".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "select-file".into(),
            opt_type: OptionType::IntegerRange,
            min: Some(1),
            max: Some(1_000_000),
            description:
                "Comma-separated list of file indices to download (BT/Metalink, 1-indexed)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- File Index Output Mapping ---
        self.register(OptionDef {
            name: "index-out".into(),
            opt_type: OptionType::IndexOut,
            short_name: Some('O'),
            cumulative_delimiter: Some("\n"),
            description: "Set output filename for BT file index (INDEX=PATH format)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- Peer Blocklist (aria2-next) ---
        self.register(OptionDef {
            name: "bt-peer-blocklist".into(),
            opt_type: OptionType::Path,
            description: "Path to BT peer blocklist file (one IP/CIDR range per line)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- uTP (UDP Transport Protocol - BEP 29) ---
        // Note: uTP is not implemented in the original C++ aria2. This is an experimental
        // feature in aria2-rust that implements BEP 29 (http://www.bittorrent.org/beps/bep_0029.html).
        // uTP provides congestion control over UDP, making BitTorrent friendlier to network traffic.
        self.register(OptionDef {
            name: "enable-utp".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Enable uTP (UDP Transport Protocol, BEP 29). Experimental feature not in original aria2. Default: false".into(),
            category: OptionCategory::BitTorrent,
            expose_in_aria2_rpc: false,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "utp-listen-port".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(0),
            min: Some(0),
            max: Some(65535),
            description: "UDP port for uTP connections. 0 = auto-assign. Experimental feature not in original aria2".into(),
            category: OptionCategory::BitTorrent,
            expose_in_aria2_rpc: false,
            ..Default::default()
        });

        // --- BT Timeouts (Internal) ---
        self.register(OptionDef {
            name: "bt-keep-alive-interval".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(120),
            min: Some(1),
            max: Some(120),
            description: "BT keep-alive interval in seconds".into(),
            category: OptionCategory::BitTorrent,
            hidden: true,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-timeout".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(180),
            min: Some(1),
            max: Some(600),
            description: "BT overall timeout in seconds".into(),
            category: OptionCategory::BitTorrent,
            hidden: true,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-request-timeout".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(60),
            min: Some(1),
            max: Some(600),
            description: "BT piece request timeout in seconds".into(),
            category: OptionCategory::BitTorrent,
            hidden: true,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "peer-connection-timeout".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(20),
            min: Some(1),
            max: Some(600),
            description: "Peer connection timeout in seconds".into(),
            category: OptionCategory::BitTorrent,
            hidden: true,
            ..Default::default()
        });

        // --- DHT Entry Points (Fine-Grained) ---
        self.register(OptionDef {
            name: "dht-entry-point-host".into(),
            opt_type: OptionType::String,
            description: "DHT bootstrap node hostname (IPv4)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dht-entry-point-port".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(0),
            min: Some(0),
            max: Some(65535),
            description: "DHT bootstrap node port (IPv4)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dht-entry-point6".into(),
            opt_type: OptionType::String,
            description: "IPv6 DHT bootstrap node (hostname:port)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dht-entry-point-host6".into(),
            opt_type: OptionType::String,
            description: "IPv6 DHT bootstrap node hostname".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dht-entry-point-port6".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(0),
            min: Some(0),
            max: Some(65535),
            description: "IPv6 DHT bootstrap node port".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dht-file-path6".into(),
            opt_type: OptionType::Path,
            description: "Path to IPv6 DHT routing table file".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dht-listen-addr".into(),
            opt_type: OptionType::String,
            description: "IPv4 DHT listen address".into(),
            category: OptionCategory::BitTorrent,
            hidden: true,
            ..Default::default()
        });
    }
}
