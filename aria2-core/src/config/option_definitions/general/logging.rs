//! Logging and progress general options: log file, log level, summary interval.

use crate::config::{OptionCategory, OptionDef, OptionType, OptionValue};

impl crate::config::OptionRegistry {
    /// Register logging and progress general options.
    pub(super) fn register_general_logging_options(&mut self) {
        // --- Logging ---
        self.register(OptionDef {
            name: "log".into(),
            opt_type: OptionType::Path,
            short_name: Some('l'),
            default_value: OptionValue::None,
            description: "Log file path".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "log-level".into(),
            opt_type: OptionType::Enum,
            default_value: OptionValue::Str("debug".into()),
            allowed_values: &["debug", "info", "notice", "warn", "error"],
            description: "Log level (debug/info/notice/warn/error)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "console-log-level".into(),
            opt_type: OptionType::Enum,
            default_value: OptionValue::Str("notice".into()),
            allowed_values: &["debug", "info", "notice", "warn", "error"],
            description: "Console log level (debug/info/notice/warn/error)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "log-backup-count".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(5),
            min: Some(1),
            description: "Number of backup log files to keep".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- Progress & Intervals ---
        self.register(OptionDef {
            name: "summary-interval".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(60),
            min: Some(0),
            max: Some(i32::MAX as u64),
            description: "Progress summary interval in seconds".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
    }
}
