use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::error::{Aria2Error, Result};
use crate::segment::Segment;

#[derive(Debug, Clone)]
pub enum DownloadStatus {
    Waiting,
    Active,
    Paused,
    Error(Aria2Error),
    Complete,
    Removed,
}

impl DownloadStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, DownloadStatus::Active)
    }

    pub fn is_completed(&self) -> bool {
        matches!(self, DownloadStatus::Complete)
    }

    pub fn is_paused(&self) -> bool {
        matches!(self, DownloadStatus::Paused)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GroupId(pub u64);

impl GroupId {
    pub fn new(id: u64) -> Self {
        GroupId(id)
    }

    pub fn value(&self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Default)]
pub struct DownloadOptions {
    pub split: Option<u16>,
    pub max_connection_per_server: Option<u16>,
    pub max_download_limit: Option<u64>,
    pub max_upload_limit: Option<u64>,
    pub dir: Option<String>,
    pub out: Option<String>,
    pub seed_time: Option<u64>,
    pub seed_ratio: Option<f64>,
    pub checksum: Option<(String, String)>,
    pub cookie_file: Option<String>,
    pub cookies: Option<String>,
    pub bt_force_encrypt: bool,
    pub bt_require_crypto: bool,
    pub enable_dht: bool,
    pub dht_listen_port: Option<u16>,
    pub enable_public_trackers: bool,
    pub bt_piece_selection_strategy: String,
    pub bt_endgame_threshold: u32,
    pub max_retries: u32,
    pub retry_wait: u64,
    pub http_proxy: Option<String>,
    pub dht_file_path: Option<String>,

    // ------------------------------------------------------------------
    // Choking algorithm configuration (BT tit-for-tat)
    // ------------------------------------------------------------------
    /// Maximum number of peers to unchoke simultaneously during seeding.
    /// Default: 4. Set to enable the choking algorithm.
    pub bt_max_upload_slots: Option<u32>,

    /// Interval in seconds between optimistic unchokes.
    /// Default: 30.
    pub bt_optimistic_unchoke_interval: Option<u64>,

    /// Timeout in seconds after which a peer is considered snubbed (not sending data).
    /// Default: 60.
    pub bt_snubbed_timeout: Option<u64>,
}

pub struct RequestGroup {
    gid: GroupId,
    uris: Vec<String>,
    options: DownloadOptions,
    status: Arc<RwLock<DownloadStatus>>,
    segments: Arc<RwLock<Vec<Segment>>>,
    total_length: u64,
    completed_length: Arc<RwLock<u64>>,
    download_speed: Arc<RwLock<u64>>,
    upload_speed: Arc<RwLock<u64>>,
    start_time: Arc<RwLock<Option<std::time::Instant>>>,
    end_time: Arc<RwLock<Option<std::time::Instant>>>,
    // New progress tracking fields (for session persistence)
    pub completed_length_atomic: AtomicU64,
    pub total_length_atomic: AtomicU64,
    pub uploaded_length: AtomicU64,
    pub download_speed_cached: AtomicU64,
    pub bt_bitfield: RwLock<Option<Vec<u8>>>,
}

impl RequestGroup {
    pub fn new(gid: GroupId, uris: Vec<String>, options: DownloadOptions) -> Self {
        info!("创建请求组 #{}", gid.value());

        RequestGroup {
            gid,
            uris,
            options,
            status: Arc::new(RwLock::new(DownloadStatus::Waiting)),
            segments: Arc::new(RwLock::new(Vec::new())),
            total_length: 0,
            completed_length: Arc::new(RwLock::new(0)),
            download_speed: Arc::new(RwLock::new(0)),
            upload_speed: Arc::new(RwLock::new(0)),
            start_time: Arc::new(RwLock::new(None)),
            end_time: Arc::new(RwLock::new(None)),
            // Initialize new progress tracking fields
            completed_length_atomic: AtomicU64::new(0),
            total_length_atomic: AtomicU64::new(0),
            uploaded_length: AtomicU64::new(0),
            download_speed_cached: AtomicU64::new(0),
            bt_bitfield: RwLock::new(None),
        }
    }

    pub async fn start(&mut self) -> Result<()> {
        let mut status = self.status.write().await;
        let mut start_time = self.start_time.write().await;

        *status = DownloadStatus::Active;
        *start_time = Some(std::time::Instant::now());

        info!("启动下载任务 #{}", self.gid.value());
        Ok(())
    }

    pub async fn pause(&mut self) -> Result<()> {
        let mut status = self.status.write().await;

        if matches!(*status, DownloadStatus::Active) {
            *status = DownloadStatus::Paused;
            info!("暂停下载任务 #{}", self.gid.value());
        }

        Ok(())
    }

    pub async fn remove(&mut self) -> Result<()> {
        let mut status = self.status.write().await;
        let mut end_time = self.end_time.write().await;

        *status = DownloadStatus::Removed;
        *end_time = Some(std::time::Instant::now());

        info!("移除下载任务 #{}", self.gid.value());
        Ok(())
    }

    pub async fn complete(&mut self) -> Result<()> {
        let mut status = self.status.write().await;
        let mut end_time = self.end_time.write().await;
        let mut completed_length = self.completed_length.write().await;

        *status = DownloadStatus::Complete;
        *end_time = Some(std::time::Instant::now());
        *completed_length = self.total_length;

        info!("完成下载任务 #{}", self.gid.value());
        Ok(())
    }

    pub async fn error(&mut self, err: Aria2Error) -> Result<()> {
        let mut status = self.status.write().await;
        let mut end_time = self.end_time.write().await;

        *status = DownloadStatus::Error(err);
        *end_time = Some(std::time::Instant::now());

        debug!("下载任务 #{} 发生错误", self.gid.value());
        Ok(())
    }

    pub async fn status(&self) -> DownloadStatus {
        self.status.read().await.clone()
    }

    pub fn gid(&self) -> GroupId {
        self.gid
    }

    pub fn uris(&self) -> &[String] {
        &self.uris
    }

    pub fn options(&self) -> &DownloadOptions {
        &self.options
    }

    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    pub async fn set_total_length(&mut self, length: u64) {
        self.total_length = length;
        debug!("设置总长度: {} bytes", length);
    }

    pub async fn completed_length(&self) -> u64 {
        *self.completed_length.read().await
    }

    pub async fn update_completed_length(&self, length: u64) {
        let mut completed_length = self.completed_length.write().await;
        *completed_length = length;
    }

    pub async fn update_progress(&self, completed_length: u64) {
        let mut cl = self.completed_length.write().await;
        *cl = completed_length;
    }

    pub async fn progress(&self) -> f64 {
        let total = self.total_length;
        let completed = *self.completed_length.read().await;

        if total == 0 {
            0.0
        } else {
            (completed as f64 / total as f64) * 100.0
        }
    }

    pub async fn download_speed(&self) -> u64 {
        *self.download_speed.read().await
    }

    pub async fn upload_speed(&self) -> u64 {
        *self.upload_speed.read().await
    }

    pub async fn update_speed(&self, dl_speed: u64, ul_speed: u64) {
        let mut ds = self.download_speed.write().await;
        let mut us = self.upload_speed.write().await;
        *ds = dl_speed;
        *us = ul_speed;
    }

    pub async fn add_segment(&mut self, segment: Segment) {
        let mut segments = self.segments.write().await;
        segments.push(segment);
        debug!("添加分段, 当前段数: {}", segments.len());
    }

    pub async fn segments(&self) -> Vec<Segment> {
        self.segments.read().await.clone()
    }

    pub async fn elapsed_time(&self) -> Option<std::time::Duration> {
        let start = *self.start_time.read().await;
        start.map(|t| t.elapsed())
    }

    pub async fn eta(&self) -> Option<std::time::Duration> {
        let speed = *self.download_speed.read().await;
        let remaining = self
            .total_length
            .saturating_sub(*self.completed_length.read().await);

        if speed == 0 || remaining == 0 {
            None
        } else {
            Some(std::time::Duration::from_secs(remaining / speed))
        }
    }

    // New progress tracking methods (for session persistence)
    // These use AtomicU64 for lock-free reads, suitable for frequent polling

    /// Set completed length using atomic store (lock-free)
    pub fn set_completed_length(&self, val: u64) {
        self.completed_length_atomic.store(val, Ordering::Relaxed);
    }

    /// Get completed length using atomic load (lock-free)
    pub fn get_completed_length(&self) -> u64 {
        self.completed_length_atomic.load(Ordering::Relaxed)
    }

    /// Set total length using atomic store (lock-free)
    pub fn set_total_length_atomic(&self, val: u64) {
        self.total_length_atomic.store(val, Ordering::Relaxed);
    }

    /// Get total length using atomic load (lock-free)
    pub fn get_total_length_atomic(&self) -> u64 {
        self.total_length_atomic.load(Ordering::Relaxed)
    }

    /// Set uploaded length using atomic store (lock-free)
    pub fn set_uploaded_length(&self, val: u64) {
        self.uploaded_length.store(val, Ordering::Relaxed);
    }

    /// Get uploaded length using atomic load (lock-free)
    pub fn get_uploaded_length(&self) -> u64 {
        self.uploaded_length.load(Ordering::Relaxed)
    }

    /// Set download speed cache using atomic store (lock-free)
    pub fn set_download_speed_cached(&self, val: u64) {
        self.download_speed_cached.store(val, Ordering::Relaxed);
    }

    /// Get download speed from cache using atomic load (lock-free)
    pub fn get_download_speed_cached(&self) -> u64 {
        self.download_speed_cached.load(Ordering::Relaxed)
    }

    /// Set BT bitfield (async, uses RwLock)
    pub async fn set_bt_bitfield(&self, bf: Option<Vec<u8>>) {
        *self.bt_bitfield.write().await = bf;
    }

    /// Get BT bitfield (async, uses RwLock)
    pub async fn get_bt_bitfield(&self) -> Option<Vec<u8>> {
        self.bt_bitfield.read().await.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_group_progress_fields_default() {
        // New RequestGroup should have all zeros/None defaults for progress fields
        let group = RequestGroup::new(
            GroupId::new(1),
            vec!["http://example.com/file.zip".to_string()],
            DownloadOptions::default(),
        );

        // Verify all atomic fields default to 0
        assert_eq!(
            group.get_completed_length(),
            0,
            "completed_length_atomic should default to 0"
        );
        assert_eq!(
            group.get_total_length_atomic(),
            0,
            "total_length_atomic should default to 0"
        );
        assert_eq!(
            group.get_uploaded_length(),
            0,
            "uploaded_length should default to 0"
        );
        assert_eq!(
            group.get_download_speed_cached(),
            0,
            "download_speed_cached should default to 0"
        );
    }

    #[test]
    fn test_set_get_completed_length() {
        let group = RequestGroup::new(
            GroupId::new(2),
            vec!["http://test.com/file.bin".to_string()],
            DownloadOptions::default(),
        );

        // Test set/get roundtrip
        group.set_completed_length(1024);
        assert_eq!(
            group.get_completed_length(),
            1024,
            "Should return 1024 after setting"
        );

        // Test update to different value
        group.set_completed_length(2048);
        assert_eq!(
            group.get_completed_length(),
            2048,
            "Should return 2048 after update"
        );

        // Test large value
        group.set_completed_length(u64::MAX);
        assert_eq!(
            group.get_completed_length(),
            u64::MAX,
            "Should handle u64::MAX"
        );

        // Test zero
        group.set_completed_length(0);
        assert_eq!(group.get_completed_length(), 0, "Should handle 0");
    }

    #[test]
    fn test_set_get_total_length() {
        let group = RequestGroup::new(
            GroupId::new(3),
            vec!["http://example.com/large.iso".to_string()],
            DownloadOptions::default(),
        );

        // Test set/get roundtrip
        group.set_total_length_atomic(1048576); // 1MB
        assert_eq!(
            group.get_total_length_atomic(),
            1048576,
            "Should return 1MB after setting"
        );

        // Test update
        group.set_total_length_atomic(1073741824); // 1GB
        assert_eq!(
            group.get_total_length_atomic(),
            1073741824,
            "Should return 1GB after update"
        );
    }

    #[tokio::test]
    async fn test_set_get_bt_bitfield() {
        let group = RequestGroup::new(
            GroupId::new(4),
            vec!["magnet:?xt=urn:btih:abc123".to_string()],
            DownloadOptions::default(),
        );

        // Default should be None
        let bf = group.get_bt_bitfield().await;
        assert!(bf.is_none(), "bt_bitfield should default to None");

        // Set and retrieve bitfield
        let test_bitfield = vec![0xFF, 0xF0, 0x0F];
        group.set_bt_bitfield(Some(test_bitfield.clone())).await;
        let retrieved = group.get_bt_bitfield().await;
        assert!(
            retrieved.is_some(),
            "bt_bitfield should be Some after setting"
        );
        assert_eq!(
            retrieved.unwrap(),
            test_bitfield,
            "bitfield should match what was set"
        );

        // Set back to None
        group.set_bt_bitfield(None).await;
        let bf_none = group.get_bt_bitfield().await;
        assert!(
            bf_none.is_none(),
            "bt_bitfield should be None after clearing"
        );

        // Test with empty bitfield
        group.set_bt_bitfield(Some(vec![])).await;
        let empty_bf = group.get_bt_bitfield().await;
        assert!(empty_bf.is_some(), "empty bitfield should still be Some");
        assert!(empty_bf.unwrap().is_empty(), "bitfield should be empty vec");
    }

    #[tokio::test]
    async fn test_concurrent_access() {
        use std::sync::Arc;

        let group = Arc::new(RequestGroup::new(
            GroupId::new(5),
            vec!["http://load.test/file.dat".to_string()],
            DownloadOptions::default(),
        ));

        // Spawn multiple tasks that read/write progress concurrently
        let mut handles = Vec::new();

        for i in 0..10 {
            let g = Arc::clone(&group);
            handles.push(tokio::spawn(async move {
                // Write progress
                g.set_completed_length(i * 100);
                g.set_total_length_atomic(10000);
                g.set_uploaded_length(i * 10);
                g.set_download_speed_cached(i * 1000);

                // Read progress (should not deadlock)
                let _cl = g.get_completed_length();
                let _tl = g.get_total_length_atomic();
                let _ul = g.get_uploaded_length();
                let _ds = g.get_download_speed_cached();

                // Occasionally write bitfield (async)
                if i % 3 == 0 {
                    let bf = vec![i as u8; 8];
                    g.set_bt_bitfield(Some(bf)).await;
                    let _retrieved = g.get_bt_bitfield().await;
                }

                // Small delay to increase chance of race conditions
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }));
        }

        // Wait for all tasks to complete without deadlock
        for handle in handles {
            handle.await.expect("Task should complete without panic");
        }

        // Verify final state is consistent
        let final_cl = group.get_completed_length();
        let final_tl = group.get_total_length_atomic();
        let final_ul = group.get_uploaded_length();
        let final_ds = group.get_download_speed_cached();

        // Values should be from one of the concurrent writers (we don't know which)
        assert!(final_cl <= 900, "completed_length should be <= 900");
        assert_eq!(final_tl, 10000, "total_length should be 10000");
        assert!(final_ul <= 90, "uploaded_length should be <= 90");
        assert!(final_ds <= 9000, "download_speed should be <= 9000");
    }

    #[test]
    fn test_set_get_uploaded_length() {
        let group = RequestGroup::new(
            GroupId::new(6),
            vec!["http://seed.test/file.torrent".to_string()],
            DownloadOptions::default(),
        );

        // Test default
        assert_eq!(group.get_uploaded_length(), 0);

        // Test set/get
        group.set_uploaded_length(512);
        assert_eq!(group.get_uploaded_length(), 512);

        // Test large value
        group.set_uploaded_length(u64::MAX / 2);
        assert_eq!(group.get_uploaded_length(), u64::MAX / 2);
    }

    #[test]
    fn test_set_get_download_speed_cached() {
        let group = RequestGroup::new(
            GroupId::new(7),
            vec!["http://speed.test/large.file".to_string()],
            DownloadOptions::default(),
        );

        // Test default
        assert_eq!(group.get_download_speed_cached(), 0);

        // Test realistic download speed (e.g., 5 MB/s = 5242880 bytes/s)
        group.set_download_speed_cached(5242880);
        assert_eq!(group.get_download_speed_cached(), 5242880);

        // Test speed update (simulating periodic updates)
        group.set_download_speed_cached(10485760); // 10 MB/s
        assert_eq!(group.get_download_speed_cached(), 10485760);
    }

    #[test]
    fn test_download_options_choking_config_defaults() {
        // New DownloadOptions should have None for choking algorithm fields (opt-in)
        let opts = DownloadOptions::default();

        assert!(
            opts.bt_max_upload_slots.is_none(),
            "bt_max_upload_slots should default to None"
        );
        assert!(
            opts.bt_optimistic_unchoke_interval.is_none(),
            "bt_optimistic_unchoke_interval should default to None"
        );
        assert!(
            opts.bt_snubbed_timeout.is_none(),
            "bt_snubbed_timeout should default to None"
        );
    }

    #[test]
    fn test_download_options_choking_config_custom() {
        // Verify that custom choking config values can be set
        let mut opts = DownloadOptions::default();
        opts.bt_max_upload_slots = Some(8);
        opts.bt_optimistic_unchoke_interval = Some(15);
        opts.bt_snubbed_timeout = Some(45);

        assert_eq!(opts.bt_max_upload_slots, Some(8));
        assert_eq!(opts.bt_optimistic_unchoke_interval, Some(15));
        assert_eq!(opts.bt_snubbed_timeout, Some(45));
    }

    #[test]
    fn test_download_options_choking_config_clone() {
        // Verify choking config fields are preserved through Clone
        let mut opts = DownloadOptions::default();
        opts.bt_max_upload_slots = Some(6);
        opts.bt_optimistic_unchoke_interval = Some(20);
        opts.bt_snubbed_timeout = Some(90);

        let cloned = opts.clone();

        assert_eq!(cloned.bt_max_upload_slots, Some(6));
        assert_eq!(cloned.bt_optimistic_unchoke_interval, Some(20));
        assert_eq!(cloned.bt_snubbed_timeout, Some(90));
    }
}
