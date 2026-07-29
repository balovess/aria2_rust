//! Network general options: interface binding, DNS, event poll, server statistics.

use crate::config::{OptionCategory, OptionDef, OptionType, OptionValue};

impl crate::config::OptionRegistry {
    /// Register network-related general options: interfaces, DNS, event poll, server stats.
    pub(super) fn register_general_network_options(&mut self) {
        // --- Network Identity ---
        self.register(OptionDef {
            name: "interface".into(),
            opt_type: OptionType::String,
            description: "Network interface to bind to".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "multiple-interface".into(),
            opt_type: OptionType::String,
            description: "Comma-separated list of interfaces for multi-homed setups".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- DNS & Async DNS ---
        self.register(OptionDef {
            name: "async-dns".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Enable asynchronous DNS resolution".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "async-dns-server".into(),
            opt_type: OptionType::String,
            description: "DNS server address for async resolver".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dns-timeout".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(30),
            min: Some(1),
            max: Some(60),
            description: "DNS resolution timeout in seconds".into(),
            category: OptionCategory::General,
            hidden: true,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "enable-async-dns6".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Enable IPv6 async DNS resolution (deprecated)".into(),
            category: OptionCategory::General,
            deprecated: true,
            ..Default::default()
        });

        // --- Network / Event Poll ---
        self.register(OptionDef {
            name: "event-poll".into(),
            opt_type: OptionType::Enum,
            default_value: OptionValue::Str("select".into()),
            description: "Event poll method (epoll/kqueue/port/poll/select)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- Server Statistics ---
        self.register(OptionDef {
            name: "server-stat-timeout".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(86400),
            min: Some(0),
            description: "Server stat timeout in seconds (0=unlimited)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "server-stat-if".into(),
            opt_type: OptionType::Path,
            description: "Server performance statistics input file".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "server-stat-of".into(),
            opt_type: OptionType::Path,
            description: "Server performance statistics output file".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
    }
}
