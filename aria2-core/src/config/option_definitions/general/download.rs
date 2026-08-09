//! Download behavior general options: resume, integrity, concurrency, limits, event hooks,
//! URI selection, mmap, checksum, resource limits.

use crate::config::{OptionCategory, OptionDef, OptionType, OptionValue};

impl crate::config::OptionRegistry {
    /// Register download behavior, limits, event hooks, and related general options.
    pub(super) fn register_general_download_options(&mut self) {
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
            short_name: Some('V'),
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
            short_name: Some('P'),
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
            short_name: Some('Z'),
            default_value: OptionValue::Bool(false),
            description: "Force sequential download of files".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "no-netrc".into(),
            opt_type: OptionType::Boolean,
            short_name: Some('n'),
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
            allowed_values: &["inorder", "feedback", "adaptive"],
            description: "URI selection algorithm (feedback/inorder/adaptive)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "stream-piece-selector".into(),
            opt_type: OptionType::Enum,
            default_value: OptionValue::Str("default".into()),
            allowed_values: &["default", "inorder", "random", "geom"],
            description: "Piece selection algorithm (default/inorder/geom/random)".into(),
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
        self.register(OptionDef {
            name: "on-download-complete".into(),
            opt_type: OptionType::Path,
            description: "Command on download complete".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "on-download-error".into(),
            opt_type: OptionType::Path,
            description: "Command on download error".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- Concurrency & Optimization ---
        self.register(OptionDef {
            name: "max-downloads".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(0),
            min: Some(0),
            description: "Max number of downloads to start (0=unlimited)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "optimize-concurrent-downloads".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Optimize concurrent download count based on network conditions".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "optimize-concurrent-downloads-coeffa".into(),
            opt_type: OptionType::Float,
            default_value: OptionValue::Float(5.0),
            description:
                "Coefficient A for optimize-concurrent-downloads (linear increasing factor)".into(),
            category: OptionCategory::General,
            hidden: true,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "optimize-concurrent-downloads-coeffb".into(),
            opt_type: OptionType::Float,
            default_value: OptionValue::Float(5.0),
            description:
                "Coefficient B for optimize-concurrent-downloads (linear decreasing factor)".into(),
            category: OptionCategory::General,
            hidden: true,
            ..Default::default()
        });

        // --- Mmap ---
        self.register(OptionDef {
            name: "enable-mmap".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Enable mmap for file allocation".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "max-mmap-limit".into(),
            opt_type: OptionType::Size,
            default_value: OptionValue::Int(i64::MAX),
            min: Some(0),
            description: "Max size limit for mmap (0=unlimited)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- Metadata & Pause ---
        self.register(OptionDef {
            name: "pause-metadata".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Pause downloads created from metadata".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- Resource Limits ---
        self.register(OptionDef {
            name: "rlimit-nofile".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(1024),
            min: Some(1),
            description: "Set soft limit of resource limit RLIMIT_NOFILE (open files)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- Hidden / Internal ---
        self.register(OptionDef {
            name: "select-least-used-host".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Select least used host for URI selection".into(),
            category: OptionCategory::General,
            hidden: true,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "startup-idle-time".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(10),
            min: Some(1),
            max: Some(60),
            description: "Startup idle time in seconds".into(),
            category: OptionCategory::General,
            hidden: true,
            ..Default::default()
        });

        // --- Checksum ---
        self.register(OptionDef {
            name: "checksum".into(),
            opt_type: OptionType::String,
            description: "Checksum for verification (hashType=digest format)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
    }
}
