//! BitTorrent option registration by configuration domain.

use crate::config::{OptionCategory, OptionDef, OptionType, OptionValue};

impl crate::config::OptionRegistry {
    pub(super) fn register_bt_network_options(&mut self) {
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
        self.register(OptionDef {
            name: "bt-incoming-peer-handshake-timeout".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(
                aria2_protocol::bittorrent::peer::incoming::DEFAULT_INCOMING_HANDSHAKE_READ_TIMEOUT
                    .as_secs() as i64,
            ),
            min: Some(1),
            max: Some(600),
            description: "Maximum wait per TCP read or uTP receive during an incoming BitTorrent peer handshake, in seconds".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-utp-max-connections".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(
                aria2_protocol::bittorrent::utp::DEFAULT_MAX_CONNECTIONS as i64,
            ),
            min: Some(1),
            max: Some(u64::from(u16::MAX) + 1),
            description: "Maximum simultaneous uTP connections per local endpoint".into(),
            category: OptionCategory::BitTorrent,
            expose_in_aria2_rpc: true,
            ..Default::default()
        });
        for (name, default, description) in [
            (
                "bt-utp-connect-timeout",
                aria2_protocol::bittorrent::utp::DEFAULT_CONNECT_TIMEOUT,
                "uTP connection establishment timeout in seconds",
            ),
            (
                "bt-utp-idle-timeout",
                aria2_protocol::bittorrent::utp::DEFAULT_IDLE_TIMEOUT,
                "uTP inactive connection timeout in seconds",
            ),
            (
                "bt-utp-keepalive-interval",
                aria2_protocol::bittorrent::utp::DEFAULT_KEEPALIVE_INTERVAL,
                "uTP keepalive interval in seconds",
            ),
        ] {
            self.register(OptionDef {
                name: name.into(),
                opt_type: OptionType::Integer,
                default_value: OptionValue::Int(default.as_secs() as i64),
                min: Some(1),
                max: Some(600),
                description: description.into(),
                category: OptionCategory::BitTorrent,
                expose_in_aria2_rpc: true,
                ..Default::default()
            });
        }
    }
}
