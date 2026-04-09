use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, debug};

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
        let remaining = self.total_length.saturating_sub(*self.completed_length.read().await);
        
        if speed == 0 || remaining == 0 {
            None
        } else {
            Some(std::time::Duration::from_secs(remaining / speed))
        }
    }
}
