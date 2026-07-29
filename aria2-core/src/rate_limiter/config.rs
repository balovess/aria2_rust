/// Configuration for a [`RateLimiter`](super::RateLimiter).
#[derive(Clone, Debug, Default)]
pub struct RateLimiterConfig {
    pub max_download_bytes_per_sec: Option<u64>,
    pub max_upload_bytes_per_sec: Option<u64>,
    pub download_burst_bytes: Option<u64>,
    pub upload_burst_bytes: Option<u64>,
}

impl RateLimiterConfig {
    pub fn new(download_limit: Option<u64>, upload_limit: Option<u64>) -> Self {
        Self {
            max_download_bytes_per_sec: download_limit,
            max_upload_bytes_per_sec: upload_limit,
            download_burst_bytes: None,
            upload_burst_bytes: None,
        }
    }

    pub fn with_burst(mut self, download_burst: Option<u64>, upload_burst: Option<u64>) -> Self {
        self.download_burst_bytes = download_burst;
        self.upload_burst_bytes = upload_burst;
        self
    }

    pub fn is_limited(&self) -> bool {
        self.max_download_bytes_per_sec.is_some() || self.max_upload_bytes_per_sec.is_some()
    }

    pub fn download_rate(&self) -> Option<u64> {
        self.max_download_bytes_per_sec
    }

    pub fn upload_rate(&self) -> Option<u64> {
        self.max_upload_bytes_per_sec
    }

    pub fn download_burst(&self) -> Option<u64> {
        self.download_burst_bytes
    }

    pub fn upload_burst(&self) -> Option<u64> {
        self.upload_burst_bytes
    }
}
