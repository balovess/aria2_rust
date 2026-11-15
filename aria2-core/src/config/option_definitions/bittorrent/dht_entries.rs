//! BitTorrent option registration by configuration domain.

use crate::config::{OptionCategory, OptionDef, OptionType, OptionValue};

impl crate::config::OptionRegistry {
    pub(super) fn register_bt_dht_entry_points(&mut self) {
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
