//! UI general options: quiet mode, color output, download result display, console readout.

use crate::config::{OptionCategory, OptionDef, OptionType, OptionValue};

impl crate::config::OptionRegistry {
    /// Register UI and display general options.
    pub(super) fn register_general_ui_options(&mut self) {
        // --- UI Behavior ---
        self.register(OptionDef {
            name: "enable-color".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Enable colored output".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "quiet".into(),
            opt_type: OptionType::Boolean,
            short_name: Some('q'),
            default_value: OptionValue::Bool(false),
            description: "Quiet mode".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dry-run".into(),
            opt_type: OptionType::Boolean,
            short_name: Some('n'),
            default_value: OptionValue::Bool(false),
            description: "Dry run (check only, no download)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- UI & Output ---
        self.register(OptionDef {
            name: "download-result".into(),
            opt_type: OptionType::Enum,
            default_value: OptionValue::Str("default".into()),
            allowed_values: &["default", "full", "hide"],
            description: "Download result output format (default/full/hide)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "human-readable".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Display file sizes in human-readable format".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "keep-unfinished-download-result".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Keep result of unfinished downloads in results list".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "truncate-console-readout".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Truncate console readout to fit terminal width".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "show-console-readout".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Show console readout (download progress display)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "stderr".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Output all console messages to stderr instead of stdout".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
    }
}
