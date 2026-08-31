//! Basic general options: directory, output, configuration files, session, daemon, GID, netrc.

use crate::config::{OptionCategory, OptionDef, OptionType, OptionValue};

impl crate::config::OptionRegistry {
    /// Register basic general options: directory, output, config, session, daemon, GID, netrc.
    pub(super) fn register_general_basic_options(&mut self) {
        // --- Directory & Output ---
        self.register(OptionDef {
            name: "dir".into(),
            opt_type: OptionType::Path,
            short_name: Some('d'),
            default_value: OptionValue::Str(".".into()),
            description: "Save directory".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "out".into(),
            opt_type: OptionType::String,
            short_name: Some('o'),
            description: "Output filename".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- Configuration Files ---
        self.register(OptionDef {
            name: "conf-path".into(),
            opt_type: OptionType::Path,
            description: "Configuration file path".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "no-conf".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Disable loading of configuration file".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "update-check".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Check for updates at most once per interval".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "update-check-interval-days".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(7),
            min: Some(1),
            max: Some(365),
            description: "Minimum days between update checks".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "input-file".into(),
            opt_type: OptionType::Path,
            short_name: Some('i'),
            description: "URI input file".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- Session Management ---
        self.register(OptionDef {
            name: "save-session".into(),
            opt_type: OptionType::Path,
            description: "Session save file".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "save-session-interval".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(0),
            description: "Auto-save session interval (0=disabled)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "auto-save-interval".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(60),
            min: Some(0),
            max: Some(600),
            description: "Auto-save interval".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- Daemon Mode ---
        self.register(OptionDef {
            name: "daemon".into(),
            opt_type: OptionType::Boolean,
            short_name: Some('D'),
            default_value: OptionValue::Bool(false),
            description: "Run as a background daemon (detached process)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "pid-file".into(),
            opt_type: OptionType::Path,
            description: "Path to PID file for daemon process management".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- GID ---
        self.register(OptionDef {
            name: "gid".into(),
            opt_type: OptionType::String,
            description: "Set GID for the first download".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- Netrc ---
        self.register(OptionDef {
            name: "netrc-path".into(),
            opt_type: OptionType::Path,
            default_value: OptionValue::Str("~/.netrc".into()),
            description: "Path to .netrc file for authentication".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
    }
}
