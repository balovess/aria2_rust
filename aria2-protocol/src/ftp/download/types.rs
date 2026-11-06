use tokio::time::Duration;

pub(super) const DEFAULT_BUFFER_SIZE: usize = 65536;

/// FTP download configuration options
#[derive(Debug, Clone)]
pub struct FtpDownloadOptions {
    pub buffer_size: usize,
    pub resume_offset: Option<u64>,
    pub max_retries: u32,
    /// Transfer mode (binary or ASCII)
    pub binary_mode: bool,
    /// Timeout for data connection establishment
    pub data_connect_timeout: Duration,
    /// Whether to download directories recursively
    pub recursive_download: bool,
}

impl Default for FtpDownloadOptions {
    fn default() -> Self {
        Self {
            buffer_size: DEFAULT_BUFFER_SIZE,
            resume_offset: None,
            max_retries: 3,
            binary_mode: true,
            data_connect_timeout: Duration::from_secs(30),
            recursive_download: false,
        }
    }
}

/// Download progress information
#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub speed_bytes_per_sec: f64,
}

/// Result of a file download operation
#[derive(Debug, Clone)]
pub struct DownloadResult {
    pub file_path: String,
    pub bytes_downloaded: u64,
    pub total_size: Option<u64>,
    pub success: bool,
    pub average_speed_bps: f64,
    pub duration_secs: f64,
}

impl DownloadResult {
    /// Check if the download completed successfully with all bytes received
    pub fn is_complete(&self) -> bool {
        self.success
            && match self.total_size {
                Some(total) => self.bytes_downloaded >= total,
                None => self.bytes_downloaded > 0,
            }
    }

    /// Convert byte count to human-readable string
    pub fn human_readable_size(bytes: u64) -> String {
        const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
        let mut size = bytes as f64;
        let mut unit_idx = 0;
        while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
            size /= 1024.0;
            unit_idx += 1;
        }
        if unit_idx == 0 {
            format!("{} {}", bytes, UNITS[unit_idx])
        } else {
            format!("{:.2} {}", size, UNITS[unit_idx])
        }
    }
}
