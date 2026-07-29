//! Metalink general options: Metalink preferences, torrent/metalink file input, show-files.

use crate::config::{OptionCategory, OptionDef, OptionType, OptionValue};

impl crate::config::OptionRegistry {
    /// Register Metalink and torrent file input general options.
    pub(super) fn register_general_metalink_options(&mut self) {
        // --- Show Files (BT/Metalink) ---
        self.register(OptionDef {
            name: "show-files".into(),
            opt_type: OptionType::Boolean,
            short_name: Some('S'),
            default_value: OptionValue::Bool(false),
            description: "Show file list for BitTorrent/Metalink".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- Torrent & Metalink File Input ---
        self.register(OptionDef {
            name: "torrent-file".into(),
            opt_type: OptionType::Path,
            short_name: Some('T'),
            description: "Path to .torrent file".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "metalink-file".into(),
            opt_type: OptionType::Path,
            description: "Path to Metalink file".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- Metalink Options ---
        // Matches C++ PREF_METALINK_* options from prefs.h
        self.register(OptionDef {
            name: "metalink-version".into(),
            opt_type: OptionType::String,
            description: "Preferred Metalink file version".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "metalink-language".into(),
            opt_type: OptionType::String,
            description: "Preferred Metalink file language".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "metalink-os".into(),
            opt_type: OptionType::String,
            description: "Preferred Metalink file operating system".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "metalink-location".into(),
            opt_type: OptionType::String,
            description: "Preferred Metalink file location (e.g. jp, us)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "follow-metalink".into(),
            opt_type: OptionType::Enum,
            default_value: OptionValue::Str("true".into()),
            description: "Auto-handle Metalink files (true/false/mem)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "metalink-preferred-protocol".into(),
            opt_type: OptionType::Enum,
            default_value: OptionValue::Str("none".into()),
            description: "Preferred protocol for Metalink (http/https/ftp/none)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "metalink-enable-unique-protocol".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Use only unique protocols per Metalink file (skip duplicate protocols)"
                .into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "metalink-base-uri".into(),
            opt_type: OptionType::String,
            description: "Base URI for resolving relative Metalink URLs".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
    }
}
