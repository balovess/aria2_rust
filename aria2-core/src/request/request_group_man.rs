use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

use super::request_group::{DownloadOptions, DownloadStatus, GroupId, RequestGroup};
use crate::error::Result;

pub struct RequestGroupMan {
    groups: Arc<RwLock<HashMap<GroupId, Arc<RwLock<RequestGroup>>>>>,
    next_gid: Arc<RwLock<u64>>,
    global_download_limit: Arc<RwLock<Option<u64>>>,
    global_upload_limit: Arc<RwLock<Option<u64>>>,
}

impl RequestGroupMan {
    pub fn new() -> Self {
        info!("初始化请求组管理器");

        RequestGroupMan {
            groups: Arc::new(RwLock::new(HashMap::new())),
            next_gid: Arc::new(RwLock::new(1)),
            global_download_limit: Arc::new(RwLock::new(None)),
            global_upload_limit: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn add_group(&self, uris: Vec<String>, options: DownloadOptions) -> Result<GroupId> {
        let gid = self.generate_gid().await;
        let group = RequestGroup::new(gid, uris, options);

        let mut groups = self.groups.write().await;
        groups.insert(gid, Arc::new(RwLock::new(group)));

        info!("添加下载任务 #{}", gid.value());
        debug!("当前任务总数: {}", groups.len());

        Ok(gid)
    }

    pub async fn remove_group(&self, gid: GroupId) -> Result<()> {
        let mut groups = self.groups.write().await;

        if let Some(group_lock) = groups.remove(&gid) {
            let mut group = group_lock.write().await;
            group.remove().await?;
            info!("移除下载任务 #{}", gid.value());
            debug!("剩余任务数: {}", groups.len());
        }

        Ok(())
    }

    pub async fn pause_group(&self, gid: GroupId) -> Result<()> {
        let groups = self.groups.read().await;

        if let Some(group_lock) = groups.get(&gid) {
            let mut group = group_lock.write().await;
            group.pause().await?;
            info!("暂停下载任务 #{}", gid.value());
        }

        Ok(())
    }

    pub async fn unpause_group(&self, gid: GroupId) -> Result<()> {
        let groups = self.groups.read().await;

        if let Some(group_lock) = groups.get(&gid) {
            let mut group = group_lock.write().await;
            if group.status().await.is_paused() {
                group.start().await?;
                info!("恢复下载任务 #{}", gid.value());
            }
        }

        Ok(())
    }

    pub async fn get_group(&self, gid: GroupId) -> Option<Arc<RwLock<RequestGroup>>> {
        let groups = self.groups.read().await;
        groups.get(&gid).cloned()
    }

    pub async fn list_groups(&self) -> Vec<Arc<RwLock<RequestGroup>>> {
        let groups = self.groups.read().await;
        groups.values().cloned().collect()
    }

    pub async fn get_active_groups(&self) -> Vec<Arc<RwLock<RequestGroup>>> {
        let groups = self.groups.read().await;
        let mut active = Vec::new();

        for group_lock in groups.values() {
            let group = group_lock.read().await;
            if group.status().await.is_active() {
                active.push(group_lock.clone());
            }
        }

        active
    }

    pub async fn get_waiting_groups(&self) -> Vec<Arc<RwLock<RequestGroup>>> {
        let groups = self.groups.read().await;
        let mut waiting = Vec::new();

        for group_lock in groups.values() {
            let group = group_lock.read().await;
            if matches!(group.status().await, DownloadStatus::Waiting) {
                waiting.push(group_lock.clone());
            }
        }

        waiting
    }

    pub async fn count(&self) -> usize {
        let groups = self.groups.read().await;
        groups.len()
    }

    pub async fn active_count(&self) -> usize {
        self.get_active_groups().await.len()
    }

    pub async fn set_global_speed_limit(
        &self,
        download_limit: Option<u64>,
        upload_limit: Option<u64>,
    ) {
        *self.global_download_limit.write().await = download_limit;
        *self.global_upload_limit.write().await = upload_limit;

        debug!(
            "设置全局速度限制 - 下载: {:?}, 上传: {:?}",
            download_limit, upload_limit
        );
    }

    pub async fn global_download_limit(&self) -> Option<u64> {
        *self.global_download_limit.read().await
    }

    pub async fn global_upload_limit(&self) -> Option<u64> {
        *self.global_upload_limit.read().await
    }

    async fn generate_gid(&self) -> GroupId {
        let mut next_gid = self.next_gid.write().await;
        let gid = GroupId(*next_gid);
        *next_gid += 1;
        gid
    }

    pub async fn clear_completed(&self) -> Result<usize> {
        let mut groups = self.groups.write().await;
        let to_remove: Vec<GroupId> = groups
            .iter()
            .filter(|(_, group_lock)| {
                futures::executor::block_on(async {
                    let group = group_lock.read().await;
                    matches!(
                        group.status().await,
                        DownloadStatus::Complete | DownloadStatus::Error(_)
                    )
                })
            })
            .map(|(gid, _)| *gid)
            .collect();

        for gid in &to_remove {
            groups.remove(gid);
        }

        info!("清除 {} 个已完成的任务", to_remove.len());
        Ok(to_remove.len())
    }
}
