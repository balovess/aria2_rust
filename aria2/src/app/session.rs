//! Session management for download persistence
//!
//! This module handles saving and restoring download sessions:
//! - Restoring incomplete downloads from session files
//! - Saving session state on shutdown
//! - Mapping session entries to download options

use super::App;
use aria2_core::request::request_group::DownloadOptions;
use aria2_core::session::active_session::ActiveSessionManager;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{debug, info, warn};

impl App {
    /// Restore incomplete download tasks from a session file.
    ///
    /// This method is called at startup to resume downloads from the
    /// --input-file session file.
    ///
    /// # Restore Logic
    /// 1. Skip entries with status "complete"
    /// 2. Skip entries with both completed_length and total_length as 0
    /// 3. Recreate download tasks for entries with progress
    /// 4. BT download bitfield info is preserved for later use
    ///
    /// # Returns
    /// - `Ok(usize)`: Number of successfully restored tasks
    /// - `Err(String)`: Error during restoration
    pub async fn restore_session(&self) -> std::result::Result<usize, String> {
        let input_file = match self.get_opt_str("input-file").await {
            Some(path) => path,
            None => return Ok(0), // No input-file specified
        };

        let session_path = PathBuf::from(&input_file);
        if !session_path.exists() {
            info!("会话文件不存在，跳过恢复: {}", input_file);
            return Ok(0);
        }

        info!("正在从会话文件恢复下载任务: {}", input_file);

        let mgr = ActiveSessionManager::new(
            session_path.clone(),
            Duration::from_secs(60), // Default interval, not used during restore
        );

        let entries = match mgr.load_session().await {
            Ok(entries) => entries,
            Err(e) => {
                warn!("加载会话文件失败: {}", e);
                return Err(e);
            }
        };

        if entries.is_empty() {
            info!("会话文件为空或无可恢复条目");
            return Ok(0);
        }

        let mut restored_count = 0;

        for entry in &entries {
            // Skip completed entries
            if entry.status == "complete" {
                debug!("跳过已完成条目: GID={:x}", entry.gid);
                continue;
            }

            // Skip entries without progress info
            if entry.completed_length == 0 && entry.total_length == 0 {
                debug!("跳过无进度条目: GID={:x}, URIs={:?}", entry.gid, entry.uris);
                continue;
            }

            // Map SessionEntry options to DownloadOptions
            let opts = Self::map_entry_to_download_options(&entry.options);

            info!(
                "恢复下载任务: GID={:x}, URIs={:?}, 进度={}/{}",
                entry.gid, entry.uris, entry.completed_length, entry.total_length
            );

            // Add group through RequestGroupMan
            {
                let man = self.request_man.read().await;
                match man.add_group(entry.uris.clone(), opts).await {
                    Ok(gid) => {
                        restored_count += 1;
                        info!("成功恢复任务 #{}", gid.value());

                        // Store BT bitfield if present
                        if entry.bitfield.is_some()
                            && let Some(group_lock) = man.get_group(gid).await
                        {
                            let group = group_lock.write().await;
                            *group.bt_bitfield.write().await = entry.bitfield.clone();
                            debug!(
                                "已设置 BT bitfield for GID={}, bits={}",
                                gid.value(),
                                entry.bitfield.as_ref().map(|b| b.len()).unwrap_or(0)
                            );
                        }
                    }
                    Err(e) => {
                        warn!("恢复任务失败 (GID={:x}): {}", entry.gid, e);
                    }
                }
            }
        }

        info!(
            "会话恢复完成: 共 {} 个条目, 恢复 {} 个任务",
            entries.len(),
            restored_count
        );
        Ok(restored_count)
    }

    /// Save active session on application shutdown.
    ///
    /// Called after engine finishes to save all incomplete downloads
    /// to the session file.
    ///
    /// # Returns
    /// - `Ok(Option<usize>)`: Number of saved entries (if save-session is configured)
    /// - `Err(String)`: Error during save
    pub async fn save_session_on_shutdown(&self) -> std::result::Result<Option<usize>, String> {
        let save_path = match self.get_opt_str("save-session").await {
            Some(path) => path,
            None => {
                debug!("未配置 save-session，跳过关闭保存");
                return Ok(None);
            }
        };

        info!("正在保存会话到: {}", save_path);

        let session_path = PathBuf::from(&save_path);
        let interval = self
            .get_opt_i64("save-session-interval")
            .await
            .unwrap_or(60)
            .max(1); // At least 1 second

        let mgr = ActiveSessionManager::new(session_path, Duration::from_secs(interval as u64));

        // Get all active groups
        let man = self.request_man.read().await;
        let groups = man.list_groups().await;

        if groups.is_empty() {
            info!("没有活动下载任务，不保存会话");
            return Ok(Some(0));
        }

        match mgr.save_session(&groups).await {
            Ok(n) => {
                info!("成功保存 {} 个条目到 {}", n, save_path);
                Ok(Some(n))
            }
            Err(e) => {
                warn!("保存会话失败: {}", e);
                Err(e)
            }
        }
    }

    /// Map SessionEntry options HashMap to DownloadOptions
    pub(super) fn map_entry_to_download_options(
        options: &std::collections::HashMap<String, String>,
    ) -> DownloadOptions {
        DownloadOptions {
            split: options.get("split").and_then(|v| v.parse::<u16>().ok()),
            max_connection_per_server: options
                .get("max-connection-per-server")
                .and_then(|v| v.parse::<u16>().ok()),
            max_download_limit: options
                .get("max-download-limit")
                .and_then(|v| v.parse::<u64>().ok()),
            max_upload_limit: options
                .get("max-upload-limit")
                .and_then(|v| v.parse::<u64>().ok()),
            dir: options.get("dir").cloned(),
            out: options.get("out").cloned(),
            seed_time: options.get("seed-time").and_then(|v| v.parse::<u64>().ok()),
            seed_ratio: options
                .get("seed-ratio")
                .and_then(|v| v.parse::<f64>().ok()),
            checksum: options.get("checksum").and_then(|v| {
                if let Some((algo, val)) = v.split_once('=') {
                    Some((algo.trim().to_string(), val.trim().to_string()))
                } else {
                    None
                }
            }),
            cookie_file: options.get("cookie-file").cloned(),
            cookies: options.get("cookies").cloned(),
            bt_force_encrypt: options
                .get("bt-force-encrypt")
                .map(|v| v == "true")
                .unwrap_or(false),
            bt_require_crypto: options
                .get("bt-require-crypto")
                .map(|v| v == "true")
                .unwrap_or(false),
            enable_dht: options
                .get("enable-dht")
                .map(|v| v != "false")
                .unwrap_or(true),
            dht_listen_port: options
                .get("dht-listen-port")
                .and_then(|v| v.parse::<u16>().ok()),
            dht_entry_point: options.get("dht-entry-point").and_then(|v| {
                if v.is_empty() {
                    None
                } else {
                    Some(v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
                }
            }),
            enable_public_trackers: options
                .get("enable-public-trackers")
                .map(|v| v != "false")
                .unwrap_or(true),
            bt_piece_selection_strategy: options
                .get("bt-piece-selection-strategy")
                .cloned()
                .unwrap_or_else(|| "rarest-first".to_string()),
            bt_endgame_threshold: options
                .get("bt-endgame-threshold")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(20),
            max_retries: options
                .get("max-retries")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(3),
            retry_wait: options
                .get("retry-wait")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(1),
            http_proxy: options.get("http-proxy").cloned(),
            all_proxy: options.get("all-proxy").cloned(),
            https_proxy: options.get("https-proxy").cloned(),
            ftp_proxy: options.get("ftp-proxy").cloned(),
            no_proxy: options.get("no-proxy").cloned(),
            dht_file_path: options.get("dht-file-path").cloned(),
            bt_max_upload_slots: options
                .get("bt-max-upload-slots")
                .and_then(|v| v.parse::<u32>().ok()),
            bt_optimistic_unchoke_interval: options
                .get("bt-optimistic-unchoke-interval")
                .and_then(|v| v.parse::<u64>().ok()),
            bt_snubbed_timeout: options
                .get("bt-snubbed-timeout")
                .and_then(|v| v.parse::<u64>().ok()),
            // G2: Piece selection priority mode
            bt_prioritize_piece: options
                .get("bt-prioritize-piece")
                .cloned()
                .unwrap_or_else(|| "rarest".to_string()),
        }
    }
}
