use colored::Colorize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};

use aria2_core::config::{ConfigManager, OptionCategory, OptionRegistry, OptionValue};
use aria2_core::engine::bt_download_command::BtDownloadCommand;
use aria2_core::engine::command::Command;
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::engine::ftp_download_command::FtpDownloadCommand;
use aria2_core::engine::magnet_download_command::MagnetDownloadCommand;
use aria2_core::engine::metalink_download_command::MetalinkDownloadCommand;
use aria2_core::engine::sftp_download_command::SftpDownloadCommand;
use aria2_core::init_logging;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_core::session::active_session::ActiveSessionManager;
use aria2_core::validation::protocol_detector::{DetectedInput, InputType, detect};
use tracing::{Level, debug, error, info, warn};

/// Top-level application runtime for aria2-rust CLI.
///
/// `App` encapsulates the complete download lifecycle:
///
/// 1. **Configuration** — `ConfigManager` with 4-source option merging
/// 2. **Engine** — `DownloadEngine` event loop for command execution
/// 3. **Request management** — `RequestGroupMan` for task lifecycle
/// 4. **UI** — Progress display, status panel, and logging
///
/// # Example
///
/// ```rust,no_run
/// use aria2::app::App;
///
/// #[tokio::main]
/// async fn main() {
///     let exit_code = App::new()
///         .run(&["--dir=/downloads", "http://example.com/file.zip".into()])
///         .await;
///     std::process::exit(exit_code);
/// }
/// ```
pub struct App {
    pub config: Arc<RwLock<ConfigManager>>,
    engine: Arc<Mutex<Option<DownloadEngine>>>,
    request_man: Arc<RwLock<RequestGroupMan>>,
    detected_inputs: Vec<DetectedInput>,
}

impl App {
    /// Create a new `App` instance with default configuration.
    pub fn new() -> Self {
        let config = Arc::new(RwLock::new(ConfigManager::new()));
        let request_man = Arc::new(RwLock::new(RequestGroupMan::new()));

        Self {
            config,
            engine: Arc::new(Mutex::new(None)),
            request_man,
            detected_inputs: Vec::new(),
        }
    }

    pub async fn load_args(&mut self, args: &[String]) -> std::result::Result<(), String> {
        let mut conf = self.config.write().await;

        let mut positional_uris = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if arg.starts_with('-') && !arg.starts_with("--") && arg.len() == 2 {
                let c = arg.chars().nth(1).unwrap_or('\0');
                if let Some(opt_name) = self.map_short_option(c) {
                    if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                        conf.set_global_option(&opt_name, OptionValue::Str(args[i + 1].clone()))
                            .await
                            .map_err(|e| format!("选项 {} 错误: {}", opt_name, e))?;
                        i += 2;
                        continue;
                    } else {
                        conf.set_global_option(&opt_name, OptionValue::Bool(true))
                            .await
                            .map_err(|e| format!("选项 {} 错误: {}", opt_name, e))?;
                        i += 1;
                        continue;
                    }
                }
            } else if arg.starts_with("--") {
                let opt_str = &arg[2..];
                if opt_str == "help" || opt_str == "h" || opt_str == "version" || opt_str == "V" {
                    i += 1;
                    continue;
                }
                let (opt_name, value) = if let Some(eq_pos) = opt_str.find('=') {
                    (&opt_str[..eq_pos], Some(&opt_str[eq_pos + 1..]))
                } else {
                    (opt_str, None)
                };

                let actual_name = if opt_name.starts_with("no-") && opt_name.len() > 3 {
                    &opt_name[3..]
                } else {
                    opt_name
                };

                if let Some(val) = value {
                    if opt_name.starts_with("no-") {
                        conf.set_global_option(actual_name, OptionValue::Bool(false))
                            .await
                            .map_err(|e| format!("选项 {} 错误: {}", opt_name, e))?;
                    } else {
                        conf.set_global_option(actual_name, OptionValue::Str(val.to_string()))
                            .await
                            .map_err(|e| format!("选项 {} 错误: {}", opt_name, e))?;
                    }
                } else if opt_name.starts_with("no-") {
                    conf.set_global_option(actual_name, OptionValue::Bool(false))
                        .await
                        .map_err(|e| format!("选项 {} 错误: {}", opt_name, e))?;
                } else if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    conf.set_global_option(opt_name, OptionValue::Str(args[i + 1].clone()))
                        .await
                        .map_err(|e| format!("选项 {} 错误: {}", opt_name, e))?;
                    i += 1;
                    i += 1;
                    continue;
                } else {
                    conf.set_global_option(opt_name, OptionValue::Bool(true))
                        .await
                        .map_err(|e| format!("选项 {} 错误: {}", opt_name, e))?;
                }

                i += 1;
                continue;
            } else if arg.starts_with('@') {
                let path = &arg[1..];
                match aria2_core::config::UriListFile::from_file(path) {
                    Ok(uri_list) => {
                        for entry in uri_list.entries() {
                            for uri in &entry.uris {
                                positional_uris.push(uri.clone());
                            }
                        }
                    }
                    Err(e) => {
                        warn!("无法加载URI列表文件 {}: {}", path, e);
                    }
                }
                i += 1;
                continue;
            } else {
                positional_uris.push(arg.clone());
            }
            i += 1;
        }

        drop(conf);

        let input_file = self.get_opt_str("input-file").await;
        if let Some(path) = input_file {
            match aria2_core::config::UriListFile::from_file(&path) {
                Ok(uri_list) => {
                    for entry in uri_list.entries() {
                        for uri in &entry.uris {
                            positional_uris.push(uri.clone());
                        }
                    }
                }
                Err(e) => {
                    warn!("无法加载input-file {}: {}", path, e);
                }
            }
        }

        self.detected_inputs = positional_uris
            .into_iter()
            .filter_map(|uri| match detect(&uri) {
                Ok(d) => Some(d),
                Err(e) => {
                    warn!("无法检测输入类型 '{}': {}", uri, e);
                    None
                }
            })
            .collect();
        Ok(())
    }

    pub async fn load_env(&mut self) {
        let mut conf = self.config.write().await;
        conf.load_env().await;
    }

    pub async fn load_config_file(
        &mut self,
        path: Option<&str>,
    ) -> std::result::Result<(), String> {
        let conf_path = if let Some(p) = path {
            p.to_string()
        } else {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| ".".to_string());

            let candidate = format!("{}/.aria2/aria2.conf", home);
            if std::path::Path::new(&candidate).exists() {
                candidate
            } else {
                return Ok(());
            }
        };

        let mut conf = self.config.write().await;
        conf.load_file(&conf_path).await;
        Ok(())
    }

    pub async fn initialize_engine(&self) {
        let tick_ms = self
            .get_opt_i64("bt-request-peer-timeout")
            .await
            .unwrap_or(100) as u64;
        let mut engine = DownloadEngine::new(tick_ms);

        let save_session_path = self
            .get_opt_str("save-session")
            .await
            .map(|s| std::path::PathBuf::from(s));
        let save_session_interval = self
            .get_opt_i64("save-session-interval")
            .await
            .and_then(|v| {
                if v > 0 {
                    Some(std::time::Duration::from_secs(v as u64))
                } else {
                    None
                }
            });

        if let Some(path) = save_session_path {
            engine.set_save_session(path, save_session_interval, self.request_man.clone());
        }

        *self.engine.lock().await = Some(engine);
        info!("引擎初始化完成");
    }

    /// 从会话文件加载并恢复未完成的下载任务
    ///
    /// 此方法在启动时调用，用于从 --input-file 指定的会话文件中恢复之前中断的下载。
    ///
    /// # 恢复逻辑
    /// 1. 跳过状态为 "complete" 的条目（已完成的不需要恢复）
    /// 2. 跳过 completed_length 和 total_length 都为 0 的条目（无进度信息）
    /// 3. 对于有进度的条目，重新创建下载任务
    /// 4. BT 下载的 bitfield 信息会被保留供后续使用
    ///
    /// # 返回值
    /// - `Ok(usize)`: 成功恢复的任务数量
    /// - `Err(String)`: 恢复过程中的错误信息
    pub async fn restore_session(&self) -> std::result::Result<usize, String> {
        let input_file = match self.get_opt_str("input-file").await {
            Some(path) => path,
            None => return Ok(0), // 未指定 input-file，无需恢复
        };

        let session_path = PathBuf::from(&input_file);
        if !session_path.exists() {
            info!("会话文件不存在，跳过恢复: {}", input_file);
            return Ok(0);
        }

        info!("正在从会话文件恢复下载任务: {}", input_file);

        let mgr = ActiveSessionManager::new(
            session_path.clone(),
            Duration::from_secs(60), // 默认间隔，恢复时不启用自动保存
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
            // 跳过已完成的条目
            if entry.status == "complete" {
                debug!("跳过已完成条目: GID={:x}", entry.gid);
                continue;
            }

            // 跳过无进度信息的条目（可能是新添加但尚未开始下载的）
            if entry.completed_length == 0 && entry.total_length == 0 {
                debug!("跳过无进度条目: GID={:x}, URIs={:?}", entry.gid, entry.uris);
                continue;
            }

            // 将 SessionEntry 的 options 映射回 DownloadOptions
            let opts = Self::map_entry_to_download_options(&entry.options);

            info!(
                "恢复下载任务: GID={:x}, URIs={:?}, 进度={}/{}",
                entry.gid, entry.uris, entry.completed_length, entry.total_length
            );

            // 通过 RequestGroupMan 添加组
            {
                let man = self.request_man.read().await;
                match man.add_group(entry.uris.clone(), opts).await {
                    Ok(gid) => {
                        restored_count += 1;
                        info!("成功恢复任务 #{}", gid.value());

                        // 如果有 BT bitfield，将其存储到 RequestGroup 中供后续使用
                        if entry.bitfield.is_some() {
                            if let Some(group_lock) = man.get_group(gid).await {
                                let group = group_lock.write().await;
                                *group.bt_bitfield.write().await = entry.bitfield.clone();
                                debug!(
                                    "已设置 BT bitfield for GID={}, bits={}",
                                    gid.value(),
                                    entry.bitfield.as_ref().map(|b| b.len()).unwrap_or(0)
                                );
                            }
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

    /// 在应用退出时保存当前活动会话
    ///
    /// 此方法应在引擎运行结束后调用，用于将所有未完成的下载任务保存到会话文件中。
    ///
    /// # 返回值
    /// - `Ok(Option<usize>)`: 成功保存的条目数量（如果配置了 save-session）
    /// - `Err(String)`: 保存失败时的错误信息
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
            .max(1); // 至少 1 秒

        let mgr = ActiveSessionManager::new(session_path, Duration::from_secs(interval as u64));

        // 获取所有活动组
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

    /// 将 SessionEntry 的 options HashMap 映射回 DownloadOptions
    fn map_entry_to_download_options(
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

    pub async fn add_downloads(&self) -> std::result::Result<Vec<u64>, String> {
        if self.detected_inputs.is_empty() {
            return Err("No download inputs provided".to_string());
        }

        let dir = self.get_opt_str("dir").await;
        let out = self.get_opt_str("out").await;
        let dl_limit = self
            .get_opt_i64("max-download-limit")
            .await
            .and_then(|v| if v > 0 { Some(v as u64) } else { None });
        let ul_limit = self
            .get_opt_i64("max-upload-limit")
            .await
            .and_then(|v| if v > 0 { Some(v as u64) } else { None });

        let split = self
            .get_opt_i64("split")
            .await
            .and_then(|v| if v > 0 { Some(v as u16) } else { None });
        let max_conn = self
            .get_opt_i64("max-connection-per-server")
            .await
            .and_then(|v| if v > 0 { Some(v as u16) } else { None });
        let seed_time = self
            .get_opt_i64("seed-time")
            .await
            .and_then(|v| if v > 0 { Some(v as u64) } else { None });
        let seed_ratio = self
            .get_opt_str("seed-ratio")
            .await
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|&r| r > 0.0);
        let checksum = self.get_opt_str("checksum").await.and_then(|v| {
            if let Some((algo, val)) = v.split_once('=') {
                Some((algo.trim().to_string(), val.trim().to_string()))
            } else {
                None
            }
        });

        let options = DownloadOptions {
            split,
            max_connection_per_server: max_conn,
            max_download_limit: dl_limit,
            max_upload_limit: ul_limit,
            dir: dir.clone(),
            out: out.clone(),
            seed_time,
            seed_ratio,
            checksum,
            cookie_file: self.get_opt_str("load-cookies").await,
            cookies: self.get_opt_str("cookie").await,
            bt_force_encrypt: self.get_opt_bool("bt-force-encrypt").await.unwrap_or(false),
            bt_require_crypto: self
                .get_opt_bool("bt-require-crypto")
                .await
                .unwrap_or(false),
            enable_dht: self.get_opt_bool("enable-dht").await.unwrap_or(true),
            dht_listen_port: self
                .get_opt_i64("dht-listen-port")
                .await
                .and_then(|v| if v > 0 { Some(v as u16) } else { None }),
            enable_public_trackers: self
                .get_opt_bool("enable-public-trackers")
                .await
                .unwrap_or(true),
            bt_piece_selection_strategy: self
                .get_opt_str("bt-piece-selection-strategy")
                .await
                .unwrap_or("rarest-first".to_string()),
            bt_endgame_threshold: self
                .get_opt_i64("bt-endgame-threshold")
                .await
                .and_then(|v| if v > 0 { Some(v as u32) } else { Some(20) })
                .unwrap_or(20),
            max_retries: self
                .get_opt_i64("max-retries")
                .await
                .and_then(|v| if v >= 0 { Some(v as u32) } else { Some(3) })
                .unwrap_or(3),
            retry_wait: self
                .get_opt_i64("retry-wait")
                .await
                .and_then(|v| if v > 0 { Some(v as u64) } else { Some(1) })
                .unwrap_or(1),
            http_proxy: self.get_opt_str("http-proxy").await,
            all_proxy: self.get_opt_str("all-proxy").await,
            https_proxy: self.get_opt_str("https-proxy").await,
            ftp_proxy: self.get_opt_str("ftp-proxy").await,
            no_proxy: self.get_opt_str("no-proxy").await,
            dht_file_path: self.get_opt_str("dht-file-path").await,
            // Choking algorithm configuration (opt-in)
            bt_max_upload_slots: self
                .get_opt_i64("bt-max-upload-slots")
                .await
                .and_then(|v| if v > 0 { Some(v as u32) } else { None }),
            bt_optimistic_unchoke_interval: self
                .get_opt_i64("bt-optimistic-unchoke-interval")
                .await
                .and_then(|v| if v > 0 { Some(v as u64) } else { None }),
            bt_snubbed_timeout: self
                .get_opt_i64("bt-snubbed-timeout")
                .await
                .and_then(|v| if v > 0 { Some(v as u64) } else { None }),
            // G2: Piece selection priority mode
            bt_prioritize_piece: self
                .get_opt_str("bt-prioritize-piece")
                .await
                .unwrap_or_else(|| "rarest".to_string()),
        };

        let mut engine_lock = self.engine.lock().await;
        let engine = engine_lock
            .as_mut()
            .ok_or_else(|| "Engine not initialized".to_string())?;

        let global_dl = self
            .get_opt_i64("max-overall-download-limit")
            .await
            .and_then(|v| if v > 0 { Some(v as u64) } else { None });
        let global_ul = self
            .get_opt_i64("max-overall-upload-limit")
            .await
            .and_then(|v| if v > 0 { Some(v as u64) } else { None });
        if global_dl.is_some() || global_ul.is_some() {
            use aria2_core::rate_limiter::RateLimiterConfig;
            engine.set_global_rate_limiter(RateLimiterConfig::new(global_dl, global_ul));
        }

        let mut gids = Vec::new();

        for (i, input) in self.detected_inputs.iter().enumerate() {
            let gid = GroupId::new(i as u64 + 1);

            let cmd: Box<dyn Command> = match &input.input_type {
                InputType::HttpUrl => Box::new(
                    DownloadCommand::new(gid, &input.raw, &options, dir.as_deref(), out.as_deref())
                        .map_err(|e| format!("HTTP download command failed: {}", e))?,
                ),
                InputType::FtpUrl => Box::new(
                    FtpDownloadCommand::new(
                        gid,
                        &input.raw,
                        &options,
                        dir.as_deref(),
                        out.as_deref(),
                    )
                    .map_err(|e| format!("FTP download command failed: {}", e))?,
                ),
                InputType::SftpUrl => Box::new(
                    SftpDownloadCommand::new(
                        gid,
                        &input.raw,
                        &options,
                        dir.as_deref(),
                        out.as_deref(),
                    )
                    .map_err(|e| format!("SFTP download command failed: {}", e))?,
                ),
                InputType::TorrentFile => {
                    let data = input
                        .file_data
                        .as_ref()
                        .ok_or_else(|| "Torrent file data not available".to_string())?;
                    Box::new(
                        BtDownloadCommand::new(gid, data, &options, dir.as_deref())
                            .map_err(|e| format!("BT download command failed: {}", e))?,
                    )
                }
                InputType::MetalinkFile => {
                    let data = input
                        .file_data
                        .as_ref()
                        .ok_or_else(|| "Metalink file data not available".to_string())?;
                    Box::new(
                        MetalinkDownloadCommand::new(gid, data, &options, dir.as_deref())
                            .map_err(|e| format!("Metalink download command failed: {}", e))?,
                    )
                }
                InputType::MagnetLink => Box::new(
                    MagnetDownloadCommand::new(gid, &input.raw, &options, dir.as_deref())
                        .map_err(|e| format!("Magnet download command failed: {}", e))?,
                ),
            };

            engine
                .add_command(cmd)
                .map_err(|e| format!("Failed to add command to engine: {}", e))?;
            gids.push(gid.value());
        }

        Ok(gids)
    }

    pub async fn run_engine(&self) -> std::result::Result<(), String> {
        let mut engine_lock: tokio::sync::MutexGuard<'_, Option<DownloadEngine>> =
            self.engine.lock().await;
        if let Some(engine) = engine_lock.take() {
            drop(engine_lock);
            info!("启动下载引擎, 共 {} 个任务", self.detected_inputs.len());
            let result: Result<(), _> = engine.run().await;
            result.map_err(|e| format!("引擎运行错误: {}", e))
        } else {
            Err("引擎未初始化".to_string())
        }
    }

    /// Run the complete application lifecycle.
    ///
    /// This is the main entry point that:
    /// 1. Handles `--help` / `--version` flags
    /// 2. Loads config from env → file → CLI args (4-source merge)
    /// 3. Initializes the download engine
    /// 4. **Restores session from input-file (if configured)**
    /// 5. Adds download tasks from positional URIs
    /// 6. Runs the engine event loop
    /// 7. **Saves session on shutdown (if configured)**
    ///
    /// Returns exit code: `0` = success, `1` = error.
    pub async fn run(&mut self, args: &[String]) -> i32 {
        if self.print_help_or_version(args) {
            return 0;
        }

        self.load_env().await;

        if let Err(e) = self.load_config_file(None).await {
            error!("加载配置文件失败: {}", e);
        }

        if let Err(e) = self.load_args(args).await {
            eprintln!("{}", format!("参数解析错误: {}", e).red());
            return 1;
        }

        let log_level = if self.get_opt_bool("verbose").await.unwrap_or(false) {
            Level::DEBUG
        } else {
            Level::INFO
        };
        let log_path = self.get_opt_str("log").await;
        init_logging(log_level, log_path.as_deref());

        self.print_banner();

        // 初始化引擎（必须在恢复会话之前）
        self.initialize_engine().await;

        // 步骤 4: 从会话文件恢复未完成的下载任务
        match self.restore_session().await {
            Ok(count) => {
                if count > 0 {
                    info!("成功恢复 {} 个下载任务", count);
                }
            }
            Err(e) => {
                warn!("会话恢复失败（将继续执行）: {}", e);
                // 恢复失败不阻止程序运行，只记录警告
            }
        }

        // 检查是否有任何输入（恢复的任务或命令行指定的 URI）
        let man = self.request_man.read().await;
        let has_restored_tasks = man.count().await > 0;

        if !has_restored_tasks && self.detected_inputs.is_empty() {
            eprintln!(
                "{}",
                "错误: 请提供下载URI或torrent文件路径，或使用 --input-file 恢复之前的下载".red()
            );
            return 1;
        }

        // 步骤 5: 添加命令行指定的下载任务
        if !self.detected_inputs.is_empty() {
            match self.add_downloads().await {
                Ok(gids) => {
                    info!("已添加 {} 个下载任务", gids.len());
                    for gid in &gids {
                        println!("  {} 任务 #{}", "#".cyan(), gid.to_string().yellow());
                    }
                }
                Err(e) => {
                    eprintln!("{}", format!("添加任务失败: {}", e).red());
                    return 1;
                }
            }
        } else if has_restored_tasks {
            info!("仅使用恢复的下载任务");
        }

        println!();

        // 步骤 6: 运行引擎
        let run_result = self.run_engine().await;

        // 步骤 7: 关闭时保存会话
        if let Err(e) = self.save_session_on_shutdown().await {
            warn!("关闭保存会话失败: {}", e);
            // 保存失败不影响退出码
        }

        match run_result {
            Ok(()) => {
                println!("{}", "所有任务完成!".green().bold());
                0
            }
            Err(e) => {
                eprintln!("{}", format!("下载失败: {}", e).red());
                1
            }
        }
    }

    fn print_help_or_version(&self, args: &[String]) -> bool {
        for arg in args {
            if arg == "--help" || arg == "-h" {
                self.print_help();
                return true;
            }
            if arg == "--version" || arg == "-V" || arg == "-v" {
                self.print_version();
                return true;
            }
        }
        false
    }

    fn print_banner(&self) {
        println!("{}", "aria2-rust v0.1.0".green().bold());
        println!(
            "{} {}",
            "Copyright:".blue(),
            "(C) 2024 aria2-rust contributors".white()
        );
        println!();
    }

    fn print_help(&self) {
        let reg = OptionRegistry::new();

        println!(
            "{}",
            "aria2-rust - The ultra fast download utility"
                .green()
                .bold()
        );
        println!();
        println!("{}", "Usage:".yellow());
        println!("  aria2c [options] <URL> [URL]...");
        println!("  aria2c [options] -T <torrent_file>");
        println!();

        let categories = [
            (OptionCategory::General, "General Options"),
            (OptionCategory::HttpFtp, "HTTP/FTP Options"),
            (OptionCategory::BitTorrent, "BitTorrent Options"),
            (OptionCategory::Rpc, "RPC Options"),
            (OptionCategory::Advanced, "Advanced Options"),
        ];

        for (cat, title) in &categories {
            let opts = reg.by_category(*cat);
            if opts.is_empty() {
                continue;
            }
            println!("{}", title.yellow());
            for def in &opts {
                let short = match def.short_name() {
                    Some(c) => format!("-{}, ", c),
                    None => String::from("    "),
                };
                let type_hint = self.type_hint_for(def.opt_type());
                let default = self.default_str_for(def.default_value());
                print!("  {}--{}{}", short, def.name(), type_hint);
                if !default.is_empty() {
                    print!("  (default: {})", default);
                }
                println!();
                println!("      {}", def.description());
            }
            println!();
        }

        println!("  -h, --help                    Show this help message");
        println!("  -V, --version                 Show version information");
        println!();
        println!("Examples:");
        println!("  aria2c http://example.com/file.zip");
        println!("  aria2c -o output.iso http://example.com/image.iso");
        println!("  aria2c -d /downloads -s 4 http://example.com/large.bin");
    }

    fn type_hint_for(&self, opt_type: aria2_core::config::OptionType) -> &'static str {
        use aria2_core::config::OptionType;
        match opt_type {
            OptionType::String | OptionType::Path => "=STR",
            OptionType::Integer | OptionType::Size => "=N",
            OptionType::Float => "=F",
            OptionType::Boolean => "",
            OptionType::List => "=LIST",
            OptionType::Enum => "=ENUM",
        }
    }

    fn default_str_for(&self, val: &aria2_core::config::OptionValue) -> String {
        use aria2_core::config::OptionValue;
        match val {
            OptionValue::None => String::new(),
            other => other.to_string(),
        }
    }

    fn print_version(&self) {
        println!("aria2-rust {} (Rust)", env!("CARGO_PKG_VERSION"));
        println!("Built with Rust {}", env!("CARGO_PKG_RUST_VERSION"));
        println!();
        println!("Features: default,bittorrent,rpc,http");
        println!();
        println!("Supported protocols:");
        println!("  HTTP/HTTPS  ✅");
        println!("  FTP/SFTP    ✅");
        println!("  BitTorrent  ✅");
        println!("  Metalink    ✅");
        println!("  RPC(JSON/XML/WebSocket) ✅");
    }

    async fn get_opt_str(&self, name: &str) -> Option<String> {
        self.config.read().await.get_global_str(name).await
    }

    async fn get_opt_i64(&self, name: &str) -> Option<i64> {
        self.config.read().await.get_global_i64(name).await
    }

    async fn get_opt_bool(&self, name: &str) -> Option<bool> {
        self.config.read().await.get_global_bool(name).await
    }

    fn map_short_option(&self, c: char) -> Option<&'static str> {
        match c {
            // General
            'd' => Some("dir"),
            'o' => Some("out"),
            'i' => Some("input-file"),
            'q' => Some("quiet"),
            'l' => Some("log"),
            'L' => Some("log-level"),
            'n' => Some("dry-run"),
            'S' => Some("summary-interval"),
            // HttpFtp — timeouts & retries
            't' => Some("timeout"),
            'T' => Some("connect-timeout"),
            'm' => Some("max-tries"),
            'w' => Some("retry-wait"),
            // HttpFtp — connections
            's' => Some("split"),
            'x' => Some("max-connection-per-server"),
            'k' => Some("min-split-size"),
            'c' => Some("continue"),
            // HttpFtp — proxies
            'p' => Some("all-proxy"),
            'P' => Some("http-proxy"),
            'y' => Some("https-proxy"),
            'F' => Some("ftp-proxy"),
            'N' => Some("no-proxy"),
            // HttpFtp — headers & identity
            'U' => Some("user-agent"),
            'R' => Some("referer"),
            'H' => Some("header"),
            // HttpFtp — cookies
            'C' => Some("load-cookies"),
            'V' => Some("save-cookies"),
            // HttpFtp — SSL & file handling
            'b' => Some("check-certificate"),
            'E' => Some("ca-certificate"),
            'O' => Some("allow-overwrite"),
            // BitTorrent
            'g' => Some("seed-ratio"),
            'G' => Some("seed-time"),
            'B' => Some("bt-max-peers"),
            'h' => Some("listen-port"),
            'D' => Some("enable-dht"),
            'X' => Some("bt-force-encryption"),
            'M' => Some("follow-torrent"),
            // RPC
            'e' => Some("enable-rpc"),
            'r' => Some("rpc-listen-port"),
            'I' => Some("rpc-secret"),
            // Advanced
            'j' => Some("max-concurrent-downloads"),
            'f' => Some("file-allocation"),
            'z' => Some("stop"),
            'A' => Some("max-overall-download-limit"),
            'Q' => Some("max-download-limit"),
            'W' => Some("max-overall-upload-limit"),
            'K' => Some("max-upload-limit"),
            'Z' => Some("disk-cache"),
            'Y' => Some("piece-length"),
            // Legacy / aliases (preserve existing case-insensitive fallbacks)
            'v' => Some("verbose"),
            _ => None,
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    /// 测试 1: 从会话文件加载条目
    ///
    /// 验证 restore_session() 能正确从 mock 的会话文件中加载并恢复条目
    #[tokio::test]
    async fn test_input_file_loads_entries() {
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let session_file = temp_dir.path().join("test_session.txt");

        // 创建一个包含 3 个条目的测试会话文件
        let session_content = r#"http://example.com/file1.zip
 GID=1
 TOTAL_LENGTH=1048576
 COMPLETED_LENGTH=524288
 STATUS=active
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=524288

http://example.com/file2.iso
 GID=2
 split=4
 dir=/downloads
 TOTAL_LENGTH=10485760
 COMPLETED_LENGTH=0
 STATUS=waiting
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=

ftp://server.com/bigfile.bin
 GID=3
 TOTAL_LENGTH=1073741824
 COMPLETED_LENGTH=536870912
 STATUS=paused
 ERROR_CODE=
 BITFIELD=fff00f
 NUM_PIECES=24
 PIECE_LENGTH=262144
 INFO_HASH=abc123def456
 RESUME_OFFSET=536870912
"#;

        // 写入会话文件
        tokio::fs::write(&session_file, session_content)
            .await
            .expect("写入会话文件失败");

        // 创建 App 实例并配置 input-file
        let app = App::new();
        {
            let mut conf = app.config.write().await;
            conf.set_global_option(
                "input-file",
                OptionValue::Str(session_file.to_string_lossy().to_string()),
            )
            .await
            .expect("设置 input-file 失败");
        }

        // 调用恢复方法
        let result = app.restore_session().await;

        // 验证结果
        assert!(result.is_ok(), "恢复应成功");
        let count = result.unwrap();

        // 应该恢复 2 个条目（跳过 completed_length=0 且 total_length>0 的 file2）
        // 但根据我们的逻辑：completed_length=0 && total_length=0 才跳过
        // file2: completed_length=0, total_length=10485760 -> 不跳过
        // 所以应该恢复 3 个条目（没有 complete 状态的）
        assert_eq!(count, 3, "应恢复 3 个非完成状态的条目");

        // 验证 RequestGroupMan 中有对应的组
        let man = app.request_man.read().await;
        let group_count = man.count().await;
        assert_eq!(group_count, 3, "RequestGroupMan 中应有 3 个组");
    }

    /// 测试 2: 跳过已完成的条目
    ///
    /// 验证状态为 "complete" 的条目在恢复时被正确跳过
    #[tokio::test]
    async fn test_skip_completed_entries() {
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let session_file = temp_dir.path().join("test_complete_session.txt");

        // 创建包含已完成条目的会话文件
        let session_content = r#"http://example.com/complete1.zip
 GID=1
 TOTAL_LENGTH=1048576
 COMPLETED_LENGTH=1048576
 STATUS=complete
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=1048576

http://example.com/active2.zip
 GID=2
 TOTAL_LENGTH=2048576
 COMPLETED_LENGTH=1024288
 STATUS=active
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=1024288

http://example.com/complete3.bin
 GID=3
 TOTAL_LENGTH=512
 COMPLETED_LENGTH=512
 STATUS=complete
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=512

http://example.com/paused4.iso
 GID=4
 TOTAL_LENGTH=10485760
 COMPLETED_LENGTH=5242880
 STATUS=paused
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=5242880
"#;

        tokio::fs::write(&session_file, session_content)
            .await
            .expect("写入会话文件失败");

        let app = App::new();
        {
            let mut conf = app.config.write().await;
            conf.set_global_option(
                "input-file",
                OptionValue::Str(session_file.to_string_lossy().to_string()),
            )
            .await
            .expect("设置 input-file 失败");
        }

        let result = app.restore_session().await;
        assert!(result.is_ok(), "恢复应成功");
        let count = result.unwrap();

        // 应只恢复 2 个条目（active 和 paused），跳过 2 个 complete
        assert_eq!(count, 2, "应只恢复 2 个非完成状态的条目");

        let man = app.request_man.read().await;
        let group_count = man.count().await;
        assert_eq!(group_count, 2, "RequestGroupMan 中应有 2 个组");
    }

    /// 测试 3: 关闭时保存会话
    ///
    /// 验证 save_session_on_shutdown() 在配置了 save-session 时正确保存
    #[tokio::test]
    async fn test_save_session_on_shutdown() {
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let save_file = temp_dir.path().join("shutdown_save.txt");

        let app = App::new();

        // 配置 save-session 选项
        {
            let mut conf = app.config.write().await;
            conf.set_global_option(
                "save-session",
                OptionValue::Str(save_file.to_string_lossy().to_string()),
            )
            .await
            .expect("设置 save-session 失败");
            conf.set_global_option("save-session-interval", OptionValue::Str("60".to_string()))
                .await
                .expect("设置 save-session-interval 失败");
        }

        // 添加一些下载任务到 RequestGroupMan
        let opts = DownloadOptions {
            dir: Some("/downloads".to_string()),
            ..Default::default()
        };

        {
            let man = app.request_man.read().await;
            man.add_group(
                vec!["http://example.com/file1.zip".to_string()],
                opts.clone(),
            )
            .await
            .expect("添加组 1 失败");

            man.add_group(vec!["http://mirror.com/file2.iso".to_string()], opts)
                .await
                .expect("添加组 2 失败");
        }

        // 调用关闭保存
        let result = app.save_session_on_shutdown().await;

        // 验证结果
        assert!(result.is_ok(), "保存应成功");
        let saved_count = result.expect("应有返回值");
        assert!(saved_count.is_some(), "配置了 save-session 时应返回 Some");
        assert_eq!(saved_count.unwrap(), 2, "应保存 2 个活动任务");

        // 验证文件已创建且包含正确的 URI
        assert!(save_file.exists(), "保存后会话文件应存在");

        let content = tokio::fs::read_to_string(&save_file)
            .await
            .expect("读取保存的文件失败");
        assert!(
            content.contains("http://example.com/file1.zip"),
            "文件应包含第一个 URI"
        );
        assert!(
            content.contains("http://mirror.com/file2.iso"),
            "文件应包含第二个 URI"
        );
    }

    /// 测试 4: 未配置 save-session 时不保存
    ///
    /// 验证当未配置 save-session 选项时，save_session_on_shutdown() 返回 Ok(None)
    #[tokio::test]
    async fn test_no_save_when_not_configured() {
        let app = App::new();

        // 不配置 save-session

        let result = app.save_session_on_shutdown().await;

        assert!(result.is_ok(), "未配置时应返回 Ok");
        assert!(
            result.unwrap().is_none(),
            "未配置 save-session 时应返回 None"
        );
    }

    /// 测试 5: map_entry_to_download_options 正确映射选项
    #[test]
    fn test_map_entry_to_download_options() {
        let mut options = HashMap::new();
        options.insert("split".to_string(), "8".to_string());
        options.insert("dir".to_string(), "/tmp/downloads".to_string());
        options.insert("out".to_string(), "output.bin".to_string());
        options.insert("max-download-limit".to_string(), "102400".to_string());
        options.insert("bt-force-encrypt".to_string(), "true".to_string());
        options.insert("enable-dht".to_string(), "false".to_string());

        let opts = App::map_entry_to_download_options(&options);

        assert_eq!(opts.split, Some(8), "split 应正确映射");
        assert_eq!(
            opts.dir,
            Some("/tmp/downloads".to_string()),
            "dir 应正确映射"
        );
        assert_eq!(opts.out, Some("output.bin".to_string()), "out 应正确映射");
        assert_eq!(
            opts.max_download_limit,
            Some(102400),
            "max-download-limit 应正确映射"
        );
        assert_eq!(
            opts.bt_force_encrypt, true,
            "bt-force-encrypt=true 应正确映射"
        );
        assert_eq!(opts.enable_dht, false, "enable-dht=false 应正确映射");
    }

    /// 测试 6: 会话文件不存在时的优雅处理
    #[tokio::test]
    async fn test_restore_nonexistent_session_file() {
        let app = App::new();

        // 配置指向不存在的文件
        {
            let mut conf = app.config.write().await;
            conf.set_global_option(
                "input-file",
                OptionValue::Str("/nonexistent/path/session.txt".to_string()),
            )
            .await
            .expect("设置 input-file 失败");
        }

        let result = app.restore_session().await;

        // 文件不存在时应返回 Ok(0)，不是错误
        assert!(result.is_ok(), "文件不存在时应返回 Ok");
        assert_eq!(result.unwrap(), 0, "文件不存在时应返回 0 个恢复条目");
    }

    /// 测试 7: 未配置 input-file 时不执行恢复
    #[tokio::test]
    async fn test_restore_without_input_file() {
        let app = App::new();

        // 不配置 input-file

        let result = app.restore_session().await;

        assert!(result.is_ok(), "未配置时应返回 Ok");
        assert_eq!(result.unwrap(), 0, "未配置 input-file 时应返回 0");
    }

    /// 测试 8: BT bitfield 在恢复时被保留
    #[tokio::test]
    async fn test_bt_bitfield_preserved_on_restore() {
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let session_file = temp_dir.path().join("bt_session.txt");

        // 创建带有 BT bitfield 的会话条目
        let session_content = r#"magnet:?xt=urn:btih:abc123def456
 GID=1
 TOTAL_LENGTH=104857600
 COMPLETED_LENGTH=52428800
 STATUS=active
 ERROR_CODE=
 BITFIELD=ffaabb
 NUM_PIECES=20
 PIECE_LENGTH=5242880
 INFO_HASH=abc123def456
 RESUME_OFFSET=52428800
"#;

        tokio::fs::write(&session_file, session_content)
            .await
            .expect("写入会话文件失败");

        let app = App::new();
        {
            let mut conf = app.config.write().await;
            conf.set_global_option(
                "input-file",
                OptionValue::Str(session_file.to_string_lossy().to_string()),
            )
            .await
            .expect("设置 input-file 失败");
        }

        let result = app.restore_session().await;
        assert!(result.is_ok(), "恢复应成功");
        assert_eq!(result.unwrap(), 1, "应恢复 1 个 BT 任务");

        // 验证 bitfield 被保留在 RequestGroup 中
        let man = app.request_man.read().await;
        let groups = man.list_groups().await;
        assert_eq!(groups.len(), 1, "应有 1 个组");

        let group = groups[0].read().await;
        let bitfield = group.bt_bitfield.read().await;
        assert!(bitfield.is_some(), "BT bitfield 应被保留");
        assert_eq!(
            bitfield.as_ref().unwrap(),
            &vec![0xFF, 0xAA, 0xBB],
            "bitfield 值应正确"
        );
    }

    /// 测试 9: 空会话文件的优雅处理
    #[tokio::test]
    async fn test_restore_empty_session_file() {
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let session_file = temp_dir.path().join("empty_session.txt");

        // 创建空会话文件
        tokio::fs::write(&session_file, "")
            .await
            .expect("写入空文件失败");

        let app = App::new();
        {
            let mut conf = app.config.write().await;
            conf.set_global_option(
                "input-file",
                OptionValue::Str(session_file.to_string_lossy().to_string()),
            )
            .await
            .expect("设置 input-file 失败");
        }

        let result = app.restore_session().await;
        assert!(result.is_ok(), "空文件应返回 Ok");
        assert_eq!(result.unwrap(), 0, "空文件应返回 0 个恢复条目");
    }

    /// 测试 10: 无进度信息条目被跳过
    #[tokio::test]
    async fn test_skip_entries_with_zero_progress() {
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let session_file = temp_dir.path().join("zero_progress_session.txt");

        // 创建所有条目都无进度的会话文件
        let session_content = r#"http://example.com/new1.zip
 GID=1
 TOTAL_LENGTH=0
 COMPLETED_LENGTH=0
 STATUS=active
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=

http://example.com/new2.iso
 GID=2
 TOTAL_LENGTH=0
 COMPLETED_LENGTH=0
 STATUS=waiting
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=
"#;

        tokio::fs::write(&session_file, session_content)
            .await
            .expect("写入会话文件失败");

        let app = App::new();
        {
            let mut conf = app.config.write().await;
            conf.set_global_option(
                "input-file",
                OptionValue::Str(session_file.to_string_lossy().to_string()),
            )
            .await
            .expect("设置 input-file 失败");
        }

        let result = app.restore_session().await;
        assert!(result.is_ok(), "应返回 Ok");
        assert_eq!(result.unwrap(), 0, "无进度的条目应全部被跳过");

        let man = app.request_man.read().await;
        let group_count = man.count().await;
        assert_eq!(group_count, 0, "不应添加任何组");
    }
}
