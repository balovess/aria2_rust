//! Download engine management
//!
//! This module handles the download engine lifecycle:
//! - Engine initialization
//! - Adding download tasks
//! - Running the engine event loop

use super::App;
#[cfg(feature = "bittorrent")]
use aria2_core::engine::bt_download_command::BtDownloadCommand;
use aria2_core::engine::command::Command;
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::engine::engine_command::EngineCommand;
use aria2_core::engine::ftp_download_command::FtpDownloadCommand;
#[cfg(feature = "bittorrent")]
use aria2_core::engine::magnet_download_command::MagnetDownloadCommand;
#[cfg(feature = "metalink")]
use aria2_core::engine::metalink_download_command::MetalinkDownloadCommand;
#[cfg(all(feature = "metalink", feature = "bittorrent"))]
use aria2_core::engine::metalink_to_request_group::MetalinkToRequestGroup;
#[cfg(feature = "sftp")]
use aria2_core::engine::sftp_download_command::SftpDownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use aria2_core::util::rwlock_ext::RwLockRecover;
use aria2_core::validation::protocol_detector::InputType;
use std::sync::Arc;
use tracing::info;

impl App {
    /// Initialize the download engine.
    pub async fn initialize_engine(&self) {
        let tick_ms = self
            .get_opt_i64("bt-request-peer-timeout")
            .await
            .unwrap_or(100) as u64;
        let mut engine = DownloadEngine::new(tick_ms);

        let save_session_path = self
            .get_opt_str("save-session")
            .await
            .map(std::path::PathBuf::from);
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

        engine.set_request_group_man(self.request_man.clone());
        *self.engine.lock().await = Some(engine);
        info!("Engine initialization complete");
    }

    /// Add download tasks from detected inputs.
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
            .get_opt_str("seed-time")
            .await
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|&v| v > 0.0);
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
            dht_entry_point: None,
            enable_public_trackers: self
                .get_opt_bool("enable-public-trackers")
                .await
                .unwrap_or(true),
            bt_piece_selection_strategy: self
                .get_opt_str("bt-piece-selection-strategy")
                .await
                .unwrap_or_else(|| crate::constants::DEFAULT_PIECE_STRATEGY.to_string()),
            bt_endgame_threshold: self
                .get_opt_i64("bt-endgame-threshold")
                .await
                .map(|v| {
                    if v > 0 {
                        v as u32
                    } else {
                        crate::constants::DEFAULT_BT_ENDGAME_THRESHOLD as u32
                    }
                })
                .unwrap_or(crate::constants::DEFAULT_BT_ENDGAME_THRESHOLD as u32),
            max_retries: self
                .get_opt_i64("max-retries")
                .await
                .map(|v| {
                    if v >= 0 {
                        v as u32
                    } else {
                        crate::constants::DEFAULT_MAX_RETRIES
                    }
                })
                .unwrap_or(crate::constants::DEFAULT_MAX_RETRIES),
            retry_wait: self
                .get_opt_i64("retry-wait")
                .await
                .map(|v| {
                    if v > 0 {
                        v as u64
                    } else {
                        crate::constants::DEFAULT_RETRY_WAIT_SECS
                    }
                })
                .unwrap_or(crate::constants::DEFAULT_RETRY_WAIT_SECS),
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
                .unwrap_or_else(|| crate::constants::DEFAULT_PIECE_PRIORITY.to_string()),
            // uTP (UDP Transport Protocol - BEP 29)
            enable_utp: self.get_opt_bool("enable-utp").await.unwrap_or(false),
            utp_listen_port: self
                .get_opt_i64("utp-listen-port")
                .await
                .and_then(|v| if v > 0 { Some(v as u16) } else { None }),
            header: self
                .get_opt_str("header")
                .await
                .map(|s| {
                    s.split('\n')
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            user_agent: self.get_opt_str("user-agent").await,
            referer: self.get_opt_str("referer").await,
            ..Default::default()
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

        #[cfg(feature = "metalink")]
        let mut metalink_resource_groups = Vec::new();
        #[cfg(all(feature = "metalink", feature = "bittorrent"))]
        let mut metalink_graphs = Vec::new();
        #[cfg(feature = "metalink")]
        let all_metalink_inputs = !self.detected_inputs.is_empty()
            && self
                .detected_inputs
                .iter()
                .all(|input| matches!(input.input_type, InputType::MetalinkFile));
        #[cfg(feature = "metalink")]
        if all_metalink_inputs {
            let mut gid_iter = {
                let man = self.request_man.read().await;
                (0..).map(move |_| man.next_available_gid())
            };
            for input in &self.detected_inputs {
                let data = input
                    .file_data
                    .as_deref()
                    .ok_or_else(|| "Metalink file data not available".to_string())?;
                let converter = MetalinkToRequestGroup::new();
                #[cfg(all(feature = "metalink", feature = "bittorrent"))]
                {
                    let graphs = converter
                        .create_torrent_graphs_from_bytes(data, &options, &mut gid_iter)
                        .map_err(|e| format!("Metalink graph construction failed: {}", e))?;
                    metalink_graphs.extend(graphs);
                }
                #[cfg(feature = "metalink")]
                {
                    let groups = converter
                        .create_resource_groups_from_bytes(data, &options, &mut gid_iter)
                        .map_err(|e| format!("Metalink resource construction failed: {}", e))?;
                    metalink_resource_groups.extend(groups);
                }
            }
        }
        let use_v2 = self.detected_inputs.iter().all(|input| {
            matches!(
                input.input_type,
                InputType::HttpUrl
                    | InputType::FtpUrl
                    | InputType::MagnetLink
                    | InputType::TorrentFile
                    | InputType::SftpUrl
            )
        }) && !self.detected_inputs.is_empty();
        let command_tx = engine.engine_command_sender();
        let mut gids = Vec::new();
        #[cfg(feature = "metalink")]
        let submitted_resource_groups = !metalink_resource_groups.is_empty();
        #[cfg(feature = "metalink")]
        if submitted_resource_groups {
            self.use_v2_engine
                .store(true, std::sync::atomic::Ordering::Release);
            for group in std::mem::take(&mut metalink_resource_groups) {
                let gid = group.recover().gid();
                command_tx
                    .send(EngineCommand::AddDownload { group })
                    .map_err(|e| format!("Failed to submit Metalink resource: {}", e))?;
                gids.push(gid.value());
            }
        }
        #[cfg(feature = "metalink")]
        if submitted_resource_groups && {
            #[cfg(all(feature = "metalink", feature = "bittorrent"))]
            {
                metalink_graphs.is_empty()
            }
            #[cfg(not(all(feature = "metalink", feature = "bittorrent")))]
            {
                true
            }
        } {
            return Ok(gids);
        }
        #[cfg(all(feature = "metalink", feature = "bittorrent"))]
        if !metalink_graphs.is_empty() {
            self.use_v2_engine
                .store(true, std::sync::atomic::Ordering::Release);
            for graph in metalink_graphs {
                let payload_gid = graph.payload.recover().gid();
                command_tx
                    .send(EngineCommand::AddMetalinkGraph { graph })
                    .map_err(|e| format!("Failed to submit Metalink graph: {}", e))?;
                gids.push(payload_gid.value());
            }
            return Ok(gids);
        }

        if use_v2 {
            self.use_v2_engine
                .store(true, std::sync::atomic::Ordering::Release);
            for (i, input) in self.detected_inputs.iter().enumerate() {
                #[cfg(not(feature = "bittorrent"))]
                if matches!(input.input_type, InputType::MagnetLink) {
                    return Err(
                        "BitTorrent support not enabled (compile with --features bittorrent)"
                            .to_string(),
                    );
                }

                let gid = GroupId::new(i as u64 + 1);
                let mut initial_uri = input.raw.clone();
                #[cfg(feature = "bittorrent")]
                if matches!(input.input_type, InputType::TorrentFile) {
                    initial_uri = format!("bt://{}", gid.value());
                }
                let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
                    gid,
                    vec![initial_uri],
                    options.clone(),
                )));
                #[cfg(feature = "bittorrent")]
                if matches!(input.input_type, InputType::TorrentFile) {
                    let data = input
                        .file_data
                        .clone()
                        .ok_or_else(|| "Torrent file data not available".to_string())?;
                    group.recover().set_bt_metadata_data(data);
                }
                command_tx
                    .send(EngineCommand::AddDownload { group })
                    .map_err(|e| format!("Failed to submit download group: {}", e))?;
                gids.push(gid.value());
            }
            return Ok(gids);
        }

        for (i, input) in self.detected_inputs.iter().enumerate() {
            let gid = GroupId::new(i as u64 + 1);

            let cmd: Box<dyn Command> = match &input.input_type {
                InputType::HttpUrl => {
                    let mut cmd = DownloadCommand::new(
                        gid,
                        &input.raw,
                        &options,
                        dir.as_deref(),
                        out.as_deref(),
                    )
                    .map_err(|e| format!("HTTP download command failed: {}", e))?;
                    if let Some(gl) = engine.global_rate_limiter().cloned() {
                        cmd.set_global_limiter(gl);
                    }
                    Box::new(cmd)
                }
                InputType::FtpUrl => {
                    let mut cmd = FtpDownloadCommand::new(
                        gid,
                        &input.raw,
                        &options,
                        dir.as_deref(),
                        out.as_deref(),
                    )
                    .map_err(|e| format!("FTP download command failed: {}", e))?;
                    if let Some(gl) = engine.global_rate_limiter().cloned() {
                        cmd.set_global_limiter(gl);
                    }
                    Box::new(cmd)
                }
                #[cfg(feature = "sftp")]
                InputType::SftpUrl => {
                    let mut cmd = SftpDownloadCommand::new(
                        gid,
                        &input.raw,
                        &options,
                        dir.as_deref(),
                        out.as_deref(),
                    )
                    .map_err(|e| format!("SFTP download command failed: {}", e))?;
                    if let Some(gl) = engine.global_rate_limiter().cloned() {
                        cmd.set_global_limiter(gl);
                    }
                    Box::new(cmd)
                }
                #[cfg(feature = "bittorrent")]
                InputType::TorrentFile => {
                    let data = input
                        .file_data
                        .as_ref()
                        .ok_or_else(|| "Torrent file data not available".to_string())?;
                    let mut cmd = BtDownloadCommand::new(gid, data, &options, dir.as_deref())
                        .map_err(|e| format!("BT download command failed: {}", e))?;
                    if let Some(gl) = engine.global_rate_limiter().cloned() {
                        cmd.set_global_limiter(gl);
                    }
                    Box::new(cmd)
                }
                #[cfg(feature = "metalink")]
                InputType::MetalinkFile => {
                    let data = input
                        .file_data
                        .as_ref()
                        .ok_or_else(|| "Metalink file data not available".to_string())?;
                    let mut cmd = MetalinkDownloadCommand::new(gid, data, &options, dir.as_deref())
                        .map_err(|e| format!("Metalink download command failed: {}", e))?;
                    if let Some(gl) = engine.global_rate_limiter().cloned() {
                        cmd.set_global_limiter(gl);
                    }
                    Box::new(cmd)
                }
                #[cfg(feature = "bittorrent")]
                InputType::MagnetLink => {
                    let mut cmd =
                        MagnetDownloadCommand::new(gid, &input.raw, &options, dir.as_deref())
                            .map_err(|e| format!("Magnet download command failed: {}", e))?;
                    if let Some(gl) = engine.global_rate_limiter().cloned() {
                        cmd.set_global_limiter(gl);
                    }
                    Box::new(cmd)
                }
                #[cfg(not(feature = "sftp"))]
                InputType::SftpUrl => {
                    return Err(
                        "SFTP support not enabled (compile with --features sftp)".to_string()
                    );
                }
                #[cfg(not(feature = "bittorrent"))]
                InputType::TorrentFile | InputType::MagnetLink => {
                    return Err(
                        "BitTorrent support not enabled (compile with --features bittorrent)"
                            .to_string(),
                    );
                }
                #[cfg(not(feature = "metalink"))]
                InputType::MetalinkFile => {
                    return Err(
                        "Metalink support not enabled (compile with --features metalink)"
                            .to_string(),
                    );
                }
            };

            engine
                .add_command(cmd)
                .map_err(|e| format!("Failed to add command to engine: {}", e))?;
            gids.push(gid.value());
        }

        Ok(gids)
    }

    /// Arm the `--stop` and `--stop-with-process` shutdown triggers.
    ///
    /// C++ registers these as routine commands in `DownloadEngineFactory`
    /// (`TimedHaltCommand` and `WatchProcessCommand`); here they become
    /// detached tokio tasks that push a `HaltAll` onto the engine's command
    /// channel. Both use a graceful (non-forced) halt, matching C++, which
    /// constructs them with the default `forceHalt = false`.
    async fn spawn_halt_watchers(
        &self,
        cmd_tx: tokio::sync::mpsc::UnboundedSender<
            aria2_core::engine::engine_command::EngineCommand,
        >,
    ) {
        use aria2_core::engine::halt_watchers;

        // `--stop=0` means "no timer" in C++ (`if (stopSec > 0)`).
        if let Some(secs) = self.get_opt_i64("stop").await.filter(|v| *v > 0) {
            info!("Engine will halt after {}s (--stop)", secs);
            halt_watchers::spawn_timed_halt(
                cmd_tx.clone(),
                std::time::Duration::from_secs(secs as u64),
                false,
            );
        }

        // C++ only checks `op->defined(PREF_STOP_WITH_PROCESS)`, so any PID
        // that was explicitly supplied arms the watcher.
        if let Some(pid) = self
            .get_opt_i64("stop-with-process")
            .await
            .filter(|v| *v > 0)
        {
            info!("Engine will halt when process {} exits", pid);
            halt_watchers::spawn_process_watch(cmd_tx, pid as u32, false);
        }
    }

    /// Run the download engine event loop.
    ///
    /// # Arguments
    ///
    /// * `keep_alive` - If true, the engine stays alive with no pending commands
    ///   (used for RPC listen mode).
    /// * `show_progress` - If true, periodically poll and render download
    ///   progress to stdout via the [`ConsoleProgressReporter`].
    pub async fn run_engine(
        &self,
        keep_alive: bool,
        show_progress: bool,
    ) -> std::result::Result<(), String> {
        let mut engine_lock: tokio::sync::MutexGuard<'_, Option<DownloadEngine>> =
            self.engine.lock().await;
        if let Some(mut engine) = engine_lock.take() {
            engine.set_keep_alive(keep_alive);
            // Two-stage Ctrl+C handling (mirrors C++ aria2 behavior):
            // 1st Ctrl+C: graceful halt (finish in-flight downloads, save session)
            // 2nd Ctrl+C: force halt (abort all downloads immediately)
            if let Some(tx) = engine.take_shutdown_sender() {
                let cmd_tx = engine.engine_cmd_tx();
                tokio::spawn(async move {
                    // First Ctrl+C: graceful shutdown
                    if tokio::signal::ctrl_c().await.is_ok() {
                        tracing::info!(
                            "Ctrl+C received, shutting down gracefully \
                             (press again to force halt)..."
                        );
                        let _ = tx.send(());
                    }
                    // Second Ctrl+C: force halt
                    if tokio::signal::ctrl_c().await.is_ok() {
                        tracing::warn!("Second Ctrl+C received, force halting all downloads!");
                        let _ = cmd_tx.send(
                            aria2_core::engine::engine_command::EngineCommand::ForceHaltAll {
                                reason:
                                    aria2_core::request::request_group::HaltReason::ShutdownSignal,
                            },
                        );
                    }
                    // Ignore subsequent Ctrl+C signals
                    loop {
                        let _ = tokio::signal::ctrl_c().await;
                    }
                });
            }

            // `--stop=N` / `--stop-with-process=PID` shutdown triggers.
            // Mirrors C++ `DownloadEngineFactory`, which registers a
            // `TimedHaltCommand` / `WatchProcessCommand` as routine commands.
            self.spawn_halt_watchers(engine.engine_cmd_tx()).await;

            drop(engine_lock);
            info!(
                "Starting download engine, {} tasks total",
                self.detected_inputs.len()
            );

            // Spawn the engine in a background task so we can run the progress
            // reporter concurrently.
            let use_v2_engine = self
                .use_v2_engine
                .load(std::sync::atomic::Ordering::Acquire);
            let engine_handle = tokio::spawn(async move {
                if use_v2_engine {
                    engine.run_v2().await
                } else {
                    engine.run().await
                }
            });

            // Spawn the progress reporter if requested.
            let (_reporter_stop_tx, reporter_handle) = if show_progress {
                let group_man = self.request_man.clone();
                let (mut reporter, stop_tx) =
                    crate::ui::console_progress::ConsoleProgressReporter::new(group_man);
                let handle = tokio::spawn(async move {
                    reporter.run().await;
                });
                (Some(stop_tx), Some(handle))
            } else {
                (None, None)
            };

            // Wait for the engine to complete.
            let result = engine_handle
                .await
                .map_err(|e| format!("Engine task panicked: {}", e))?
                .map_err(|e| format!("Engine runtime error: {}", e));

            // Signal the progress reporter to stop (dropping the sender is
            // sufficient to close the oneshot channel).
            drop(_reporter_stop_tx);
            if let Some(handle) = reporter_handle {
                let _ = handle.await;
            }

            result
        } else {
            Err("Engine not initialized".to_string())
        }
    }
}
