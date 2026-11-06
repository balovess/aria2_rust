use std::path::PathBuf;

use clap::Args;

// =========================================================================
// Advanced Options
// =========================================================================

/// Advanced options: bandwidth limits, disk cache, file allocation.
#[derive(Args, Debug)]
#[command(next_help_heading = "Advanced options")]
pub struct AdvancedArgs {
    /// File allocation method (none/prealloc/falloc/trunc/mmap)
    #[arg(short = 'a', long = "file-allocation")]
    pub file_allocation: Option<String>,

    /// Zero-fill allocated space after fallocate (macOS/Windows)
    #[arg(
        long = "secure-falloc",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub secure_falloc: Option<bool>,

    /// File size threshold for mmap writes (default 256M)
    #[arg(long = "mmap-threshold")]
    pub mmap_threshold: Option<String>,

    /// Max concurrent downloads
    #[arg(short = 'j', long = "max-concurrent-downloads")]
    pub max_concurrent_downloads: Option<u64>,

    /// Overall download speed limit (0=unlimited)
    #[arg(long = "max-overall-download-limit")]
    pub max_overall_download_limit: Option<String>,

    /// Per-task download limit (0=unlimited)
    #[arg(long = "max-download-limit")]
    pub max_download_limit: Option<String>,

    /// Overall upload speed limit (0=unlimited)
    #[arg(long = "max-overall-upload-limit")]
    pub max_overall_upload_limit: Option<String>,

    /// Per-task upload limit (0=unlimited)
    #[arg(short = 'u', long = "max-upload-limit")]
    pub max_upload_limit: Option<String>,

    /// BT piece length
    #[arg(long = "piece-length")]
    pub piece_length: Option<String>,

    /// Disk cache size (0=disabled)
    #[arg(long = "disk-cache")]
    pub disk_cache: Option<String>,

    /// Stop after N seconds of completion (0=never)
    #[arg(long = "stop")]
    pub stop: Option<u64>,

    /// Force save state on every change
    #[arg(
        long = "force-save",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub force_save: Option<bool>,

    /// Path to save/load server performance statistics
    #[arg(long = "server-stat-file")]
    pub server_stat_file: Option<PathBuf>,

    /// Auto-save interval for server stats in seconds (0=disabled)
    #[arg(long = "save-server-stat-interval")]
    pub save_server_stat_interval: Option<u64>,

    /// DSCP (DiffServ) IP packet marking value (0-63)
    #[arg(long = "dscp")]
    pub dscp: Option<u64>,

    /// Socket receive buffer size (0=OS default)
    #[arg(long = "socket-recv-buffer-size")]
    pub socket_recv_buffer_size: Option<String>,

    /// Max resume failure retries before a fresh download (0=unlimited)
    #[arg(long = "max-resume-failure-tries")]
    pub max_resume_failure_tries: Option<u64>,

    /// Max log file size before rotation
    #[arg(long = "log-max-size")]
    pub log_max_size: Option<String>,

    /// Max rotated log files to keep
    #[arg(long = "log-max-files")]
    pub log_max_files: Option<u64>,

    /// Optimize concurrent download count based on network conditions
    #[arg(
        long = "optimize-concurrent-downloads",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub optimize_concurrent_downloads: Option<bool>,

    /// Optimization coefficient A
    #[arg(long = "optimize-concurrent-downloads-coeffA", hide = true)]
    pub optimize_concurrent_downloads_coeff_a: Option<f64>,

    /// Optimization coefficient B
    #[arg(long = "optimize-concurrent-downloads-coeffB", hide = true)]
    pub optimize_concurrent_downloads_coeff_b: Option<f64>,
}
