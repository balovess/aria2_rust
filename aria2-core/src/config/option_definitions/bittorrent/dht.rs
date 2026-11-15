//! BitTorrent option registration by configuration domain.

use crate::config::{OptionCategory, OptionDef, OptionType, OptionValue};

impl crate::config::OptionRegistry {
    pub(super) fn register_bt_dht_options(&mut self) {
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
        for (name, default, description) in [
            (
                "dht-refresh-check-interval",
                300,
                "DHT bucket refresh check interval in seconds",
            ),
            (
                "dht-token-rotation-interval",
                600,
                "DHT token rotation interval in seconds",
            ),
            (
                "dht-node-contact-interval",
                900,
                "DHT node contact interval in seconds",
            ),
            (
                "dht-cleanup-interval",
                300,
                "DHT cleanup and eviction interval in seconds",
            ),
            (
                "dht-save-interval",
                1800,
                "DHT routing-table save interval in seconds",
            ),
            (
                "dht-bootstrap-timeout",
                60,
                "DHT bootstrap timeout in seconds",
            ),
            (
                "dht-persistence-max-age",
                24 * 60 * 60,
                "Maximum age of a persisted DHT snapshot in seconds",
            ),
        ] {
            self.register(OptionDef {
                name: name.into(),
                opt_type: OptionType::Integer,
                default_value: OptionValue::Int(default),
                min: Some(1),
                description: description.into(),
                category: OptionCategory::BitTorrent,
                ..Default::default()
            });
        }
        self.register(OptionDef {
            name: "dht-max-concurrent-lookups".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(16),
            min: Some(1),
            max: Some(1024),
            description: "Maximum concurrent DHT lookup tasks".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dht-max-peer-info-hashes".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(
                aria2_protocol::bittorrent::dht::DhtPeerStorageConfig::default().max_info_hashes
                    as i64,
            ),
            min: Some(1),
            description: "Maximum announced swarms retained by each DHT engine".into(),
            category: OptionCategory::BitTorrent,
            expose_in_aria2_rpc: true,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dht-max-peers-per-info-hash".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(
                aria2_protocol::bittorrent::dht::DhtPeerStorageConfig::default()
                    .max_peers_per_info_hash as i64,
            ),
            min: Some(1),
            description: "Maximum announced peers retained for each DHT info-hash".into(),
            category: OptionCategory::BitTorrent,
            expose_in_aria2_rpc: true,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dht-max-items".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(
                aria2_protocol::bittorrent::dht::DEFAULT_DHT_MAX_ITEMS as i64,
            ),
            min: Some(1),
            description: "Maximum BEP 44 items retained by each DHT engine".into(),
            category: OptionCategory::BitTorrent,
            expose_in_aria2_rpc: true,
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
    }
}
