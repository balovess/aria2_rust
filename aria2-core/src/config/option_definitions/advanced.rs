//! Advanced category options: bandwidth limits, disk cache, file allocation, socket tuning.

use crate::config::{OptionCategory, OptionDef, OptionType, OptionValue};

impl crate::config::OptionRegistry {
    /// Register advanced/performance options: bandwidth limits, disk cache, file allocation.
    pub fn register_advanced_options(&mut self) {
        // --- File Allocation ---
        self.register(OptionDef {
            name: "file-allocation".into(),
            opt_type: OptionType::Enum,
            short_name: Some('f'),
            default_value: OptionValue::Str("trunc".into()),
            allowed_values: &["none", "prealloc", "falloc", "trunc", "mmap"],
            description: "File allocation method (none/prealloc/falloc/trunc/mmap)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });

        // --- Secure Falloc ---
        // Zero-fill allocated space after fallocate on platforms that don't
        // zero-fill (macOS F_PREALLOCATE, Windows SetFileValidData). Prevents
        // exposure of residual disk data at a performance cost. Has no effect
        // on Linux where fallocate(2) always returns zeroed blocks.
        self.register(OptionDef {
            name: "secure-falloc".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Zero-fill allocated space after fallocate on platforms that don't zero-fill (macOS, Windows). Prevents exposure of residual disk data at a performance cost.".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });

        // --- Mmap Threshold ---
        // Files larger than this threshold use MmapDiskWriter when
        // --file-allocation=mmap is set. Below the threshold, positioned I/O
        // is used (avoids address space waste for small files).
        self.register(OptionDef {
            name: "mmap-threshold".into(),
            opt_type: OptionType::Size,
            default_value: OptionValue::Int(256 * 1024 * 1024), // 256 MiB
            description:
                "File size threshold for mmap writes when file-allocation=mmap (default 256 MiB)"
                    .into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });

        // --- Concurrency ---
        self.register(OptionDef {
            name: "max-concurrent-downloads".into(),
            opt_type: OptionType::Integer,
            short_name: Some('j'),
            default_value: OptionValue::Int(5),
            min: Some(1),
            max: Some(256),
            description: "Max concurrent downloads".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });

        // --- Bandwidth Limits ---
        self.register(OptionDef {
            name: "max-overall-download-limit".into(),
            opt_type: OptionType::Size,
            short_name: Some('A'),
            default_value: OptionValue::Int(0),
            description: "Overall download speed limit (0=unlimited)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "max-download-limit".into(),
            opt_type: OptionType::Size,
            short_name: Some('Q'),
            default_value: OptionValue::Int(0),
            description: "Per-task download limit (0=unlimited)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "max-overall-upload-limit".into(),
            opt_type: OptionType::Size,
            short_name: Some('W'),
            default_value: OptionValue::Int(0),
            description: "Overall upload speed limit (0=unlimited)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "max-upload-limit".into(),
            opt_type: OptionType::Size,
            short_name: Some('K'),
            default_value: OptionValue::Int(0),
            description: "Per-task upload limit (0=unlimited)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });

        // --- BT Piece & Disk ---
        self.register(OptionDef {
            name: "piece-length".into(),
            opt_type: OptionType::Size,
            short_name: Some('Y'),
            default_value: OptionValue::Int((1024 * 1024) as i64),
            description: "BT piece length".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "disk-cache".into(),
            opt_type: OptionType::Size,
            short_name: Some('Z'),
            default_value: OptionValue::Int((16 * 1024 * 1024) as i64),
            description: "Disk cache size (0=disabled)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });

        // --- Auto-stop & Save ---
        self.register(OptionDef {
            name: "stop".into(),
            opt_type: OptionType::Integer,
            short_name: Some('z'),
            default_value: OptionValue::Int(0),
            min: Some(0),
            max: Some(86400),
            description: "Stop after N seconds of completion (0=never)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "force-save".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Force save state on every change".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });

        // --- Server Statistics Persistence ---
        self.register(OptionDef {
            name: "server-stat-file".into(),
            opt_type: OptionType::Path,
            description: "Path to save/load server performance statistics".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "save-server-stat-interval".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(0),
            min: Some(0),
            max: Some(86400),
            description: "Auto-save interval for server stats in seconds (0 = disabled)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });

        // --- Socket & Network Tuning ---
        self.register(OptionDef {
            name: "socket-recv-buffer-size".into(),
            opt_type: OptionType::Size,
            default_value: OptionValue::Int(0),
            description: "Socket receive buffer size (0=OS default)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dscp".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(0),
            min: Some(0),
            max: Some(63),
            description: "DSCP (DiffServ) IP packet marking value (0-63)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });

        // --- Resume Robustness ---
        self.register(OptionDef {
            name: "max-resume-failure-tries".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(0),
            min: Some(0),
            description: "Max resume failure retries before downloading from scratch (0=unlimited)"
                .into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });

        // --- Log Rotation (aria2-next) ---
        self.register(OptionDef {
            name: "log-max-size".into(),
            opt_type: OptionType::Size,
            default_value: OptionValue::Int(10 * 1024 * 1024), // 10 MiB
            min: Some(1024),                                   // 1 KiB
            max: Some(1024 * 1024 * 1024),                     // 1 GiB
            description: "Max log file size in bytes before rotation (default 10 MiB)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "log-max-files".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(4),
            min: Some(1),
            description: "Max number of rotated log files to keep (default 4)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });
    }
}
