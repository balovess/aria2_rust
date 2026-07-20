//! General category options: directory, logging, UI, session, download behavior.

use crate::config::{OptionCategory, OptionDef, OptionType, OptionValue};

impl crate::config::OptionRegistry {
    /// Register general-purpose options: directory, output, logging, UI, session management.
    pub fn register_general_options(&mut self) {
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
            short_name: Some('L'),
            default_value: OptionValue::Str("debug".into()),
            description: "Log level (debug/info/notice/warn/error)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "console-log-level".into(),
            opt_type: OptionType::Enum,
            default_value: OptionValue::Str("notice".into()),
            description: "Console log level".into(),
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
            short_name: Some('S'),
            default_value: OptionValue::Int(60),
            min: Some(0),
            max: Some(3600),
            description: "Progress summary interval in seconds".into(),
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

        // --- Download Behavior ---
        self.register(OptionDef {
            name: "allow-piece-length-change".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Allow piece length change during download".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "always-resume".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Always resume download from available session data".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "check-integrity".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Check file integrity by validating hash".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "conditional-get".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Only download if newer than local file (HTTP conditional GET)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "deferred-input".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Read URIs from input file on-demand rather than at startup".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "disable-ipv6".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Disable IPv6 support entirely".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "hash-check-only".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Only check hash integrity, do not download".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "parameterized-uri".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Enable parameterized URI support (e.g. {a,b})".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "pause".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Start downloads in paused state".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "remove-control-file".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Remove control file before download".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "reuse-uri".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Reuse previously used URIs if connection fails".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "save-not-found".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Save URIs that returned 404 as not found".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "force-sequential".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Force sequential download of files".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "no-netrc".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Disable netrc file parsing for authentication".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "realtime-chunk-checksum".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Verify checksum for each chunk in real-time".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- UI & Output ---
        self.register(OptionDef {
            name: "download-result".into(),
            opt_type: OptionType::Enum,
            default_value: OptionValue::Str("default".into()),
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

        // --- Limits ---
        self.register(OptionDef {
            name: "max-download-result".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(1000),
            min: Some(0),
            description: "Max number of download results to remember".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "lowest-speed-limit".into(),
            opt_type: OptionType::Size,
            default_value: OptionValue::Int(0),
            description: "Lowest download speed limit (if below, aborts)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "max-file-not-found".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(0),
            min: Some(0),
            description: "Max number of 404 not-found attempts (0=unlimited)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "no-file-allocation-limit".into(),
            opt_type: OptionType::Size,
            default_value: OptionValue::Int((5 * 1024 * 1024) as i64),
            description: "File size limit below which no file allocation occurs".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "stop-with-process".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(0),
            min: Some(0),
            description: "Stop aria2 when process with given PID exits (0=disabled)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- URI / Selector ---
        self.register(OptionDef {
            name: "uri-selector".into(),
            opt_type: OptionType::Enum,
            default_value: OptionValue::Str("feedback".into()),
            description: "URI selection algorithm (feedback/inorder/adaptive)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "stream-piece-selector".into(),
            opt_type: OptionType::Enum,
            default_value: OptionValue::Str("default".into()),
            description: "Piece selection algorithm (default/inorder/geom/random)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

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

        // --- GID ---
        self.register(OptionDef {
            name: "gid".into(),
            opt_type: OptionType::String,
            description: "Set GID for the first download".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- Event Hooks ---
        self.register(OptionDef {
            name: "on-download-start".into(),
            opt_type: OptionType::Path,
            description: "Command on download start".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "on-download-stop".into(),
            opt_type: OptionType::Path,
            description: "Command on download stop".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "on-download-pause".into(),
            opt_type: OptionType::Path,
            description: "Command on download pause".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
    }
}
