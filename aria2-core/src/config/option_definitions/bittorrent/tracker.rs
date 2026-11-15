//! BitTorrent option registration by configuration domain.

use crate::config::{OptionCategory, OptionDef, OptionType, OptionValue};

impl crate::config::OptionRegistry {
    pub(super) fn register_bt_tracker_options(&mut self) {
        self.register(OptionDef {
            name: "bt-tracker".into(),
            opt_type: OptionType::List,
            description: "Comma-separated list of tracker announce URIs".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-tracker-source".into(),
            opt_type: OptionType::List,
            default_value: OptionValue::List(vec![
                aria2_protocol::bittorrent::tracker::public_list::DEFAULT_TRACKER_SOURCE
                    .to_string(),
            ]),
            cumulative_delimiter: Some("\n"),
            description: "Remote public tracker list sources".into(),
            category: OptionCategory::BitTorrent,
            // Rust-only extension: expose it explicitly so RPC clients can
            // inspect and change the public tracker catalog configuration.
            expose_in_aria2_rpc: true,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-tracker-update-interval".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(
                aria2_protocol::bittorrent::tracker::public_list::DEFAULT_TRACKER_UPDATE_INTERVAL
                    .as_secs() as i64,
            ),
            min: Some(1),
            description: "Public tracker list refresh interval in seconds".into(),
            category: OptionCategory::BitTorrent,
            expose_in_aria2_rpc: true,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-tracker-source-timeout".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(
                aria2_protocol::bittorrent::tracker::public_list::DEFAULT_TRACKER_SOURCE_TIMEOUT
                    .as_secs() as i64,
            ),
            min: Some(1),
            max: Some(600),
            description: "Timeout for each public tracker source download in seconds".into(),
            category: OptionCategory::BitTorrent,
            expose_in_aria2_rpc: true,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-tracker-source-concurrency".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(
                aria2_protocol::bittorrent::tracker::public_list::MAX_TRACKER_SOURCE_CONCURRENCY
                    as i64,
            ),
            min: Some(1),
            max: Some(
                aria2_protocol::bittorrent::tracker::public_list::MAX_TRACKER_SOURCE_CONCURRENCY
                    as u64,
            ),
            description: "Maximum concurrent downloads of remote public tracker lists".into(),
            category: OptionCategory::BitTorrent,
            expose_in_aria2_rpc: true,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "enable-public-trackers".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Use public trackers in addition to torrent trackers".into(),
            category: OptionCategory::BitTorrent,
            expose_in_aria2_rpc: true,
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
            name: "bt-tracker-stopped-timeout".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(
                crate::constants::BT_TRACKER_STOPPED_TIMEOUT_SECS as i64,
            ),
            min: Some(1),
            max: Some(600),
            description: "Total timeout for stopped tracker announces in seconds".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
    }
}
