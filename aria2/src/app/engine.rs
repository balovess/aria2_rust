//! Download engine management
//!
//! This module handles the download engine lifecycle:
//! - Engine initialization
//! - Adding download tasks
//! - Running the engine event loop

use super::App;
#[cfg(feature = "bittorrent")]
use aria2_core::config::TrackerCatalogConfig;
use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::engine::engine_command::EngineCommand;
#[cfg(all(feature = "metalink", feature = "bittorrent"))]
use aria2_core::engine::metalink_to_request_group::MetalinkToRequestGroup;
use aria2_core::request::request_group::{GroupId, RequestGroup};
use aria2_core::util::rwlock_ext::RwLockRecover;
use aria2_core::validation::protocol_detector::InputType;
#[cfg(feature = "metalink")]
use aria2_protocol::metalink::parser::MetalinkDocument;
use std::sync::Arc;
use tracing::info;

impl App {
    /// Initialize the download engine.
    pub async fn initialize_engine(&self) {
        #[cfg(feature = "bittorrent")]
        let mut engine = {
            let config = self.config.read().await;
            let sources = match config.get_global_option("bt-tracker-source").await {
                Some(aria2_core::config::OptionValue::List(values)) => values,
                Some(aria2_core::config::OptionValue::Str(value)) => value
                    .split([',', '\n'])
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .collect(),
                _ => Vec::new(),
            };
            let update_interval = config
                .get_global_i64("bt-tracker-update-interval")
                .await
                .filter(|seconds| *seconds > 0)
                .map(|seconds| std::time::Duration::from_secs(seconds as u64))
                .unwrap_or(
                    aria2_protocol::bittorrent::tracker::public_list::DEFAULT_TRACKER_UPDATE_INTERVAL,
                );
            let enabled = config
                .get_global_bool("enable-public-trackers")
                .await
                .unwrap_or(true);
            let lpd_port = config
                .get_global_i64("lpd-listen-port")
                .await
                .and_then(|port| u16::try_from(port).ok())
                .unwrap_or(aria2_core::constants::LPD_PORT);
            let lpd_interface = match config.get_global_option("bt-lpd-interface").await {
                Some(aria2_core::config::OptionValue::Str(value)) if !value.trim().is_empty() => {
                    match value.parse::<std::net::Ipv4Addr>() {
                        Ok(interface) => Some(interface),
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                value = %value,
                                "Ignoring invalid bt-lpd-interface"
                            );
                            None
                        }
                    }
                }
                _ => None,
            };
            let lpd_manager = match aria2_core::engine::lpd_manager::LpdManager::with_interval_and_interface_and_port(
                aria2_core::constants::LPD_DEFAULT_ANNOUNCE_INTERVAL_SECS,
                lpd_interface,
                lpd_port,
            ) {
                Ok(manager) => Arc::new(manager),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "Using default LPD manager because configured LPD setup failed"
                    );
                    Arc::new(aria2_core::engine::lpd_manager::LpdManager::new())
                }
            };

            let mut engine = DownloadEngine::with_lpd_manager(
                crate::constants::DEFAULT_TICK_INTERVAL_MS,
                lpd_manager,
            );
            engine.set_public_tracker_config(TrackerCatalogConfig {
                enabled,
                sources,
                update_interval,
            });
            engine
        };

        #[cfg(not(feature = "bittorrent"))]
        let mut engine = DownloadEngine::new(crate::constants::DEFAULT_TICK_INTERVAL_MS);

        let server_stat_timeout = self
            .get_opt_i64("server-stat-timeout")
            .await
            .filter(|value| *value >= 0)
            .map(|value| value as u64)
            .unwrap_or(24 * 60 * 60);
        let server_stat_input = self
            .get_opt_str("server-stat-if")
            .await
            .map(std::path::PathBuf::from);
        let server_stat_output = self
            .get_opt_str("server-stat-of")
            .await
            .map(std::path::PathBuf::from);
        let server_stat_interval = self
            .get_opt_i64("save-server-stat-interval")
            .await
            .filter(|value| *value > 0)
            .map(|value| std::time::Duration::from_secs(value as u64));
        engine.set_server_stat_timeout(server_stat_timeout);
        engine.set_server_stat_persistence(
            server_stat_input,
            server_stat_output,
            server_stat_interval,
        );

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
        let auto_save_interval = self
            .get_opt_i64("auto-save-interval")
            .await
            .and_then(|v| (v > 0).then_some(std::time::Duration::from_secs(v as u64)));

        engine.set_auto_save_interval(auto_save_interval);
        engine.set_request_group_man(self.request_man.clone());
        if let Some(path) = save_session_path {
            engine.set_save_session(path, save_session_interval, self.request_man.clone());
        }
        *self.engine.lock().await = Some(engine);
        info!("Engine initialization complete");
    }

    /// Add download tasks from detected inputs.
    pub async fn add_downloads(&self) -> std::result::Result<Vec<u64>, String> {
        if self.detected_inputs.is_empty() {
            return Err("No download inputs provided".to_string());
        }

        let (options, option_snapshot) = self.download_options_with_snapshot().await;
        let explicit_gid = self
            .get_opt_str("gid")
            .await
            .map(|value| {
                GroupId::from_hex_string(&value)
                    .ok_or_else(|| format!("Invalid GID '{}': expected a hexadecimal u64", value))
            })
            .transpose()?;
        let global_dl = self
            .get_opt_i64("max-overall-download-limit")
            .await
            .and_then(|v| (v > 0).then_some(v as u64));
        let global_ul = self
            .get_opt_i64("max-overall-upload-limit")
            .await
            .and_then(|v| (v > 0).then_some(v as u64));

        let mut engine_lock = self.engine.lock().await;
        let engine = engine_lock
            .as_mut()
            .ok_or_else(|| "Engine not initialized".to_string())?;

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
            // Reserve a local GID sequence before parsing/building groups.
            // The old iterator captured the async manager read guard across
            // the conversion loop, which could block RPC writes for the
            // entire Metalink conversion. Two IDs per file cover both the
            // direct-resource and metadata/payload graph paths.
            let gid_count = self
                .detected_inputs
                .iter()
                .filter_map(|input| input.file_data.as_deref())
                .map(|data| {
                    MetalinkDocument::parse(data, None)
                        .map(|document| document.files.len().saturating_mul(2))
                        .map_err(|error| format!("Metalink GID reservation failed: {error}"))
                })
                .try_fold(0usize, |total, count| {
                    count.map(|count| total.saturating_add(count))
                })?;
            let first_gid = { self.request_man.next_available_gid().value() };
            let mut gid_iter =
                (0..gid_count).map(|offset| GroupId::new(first_gid.saturating_add(offset as u64)));
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
                    for graph in &graphs {
                        graph
                            .metadata
                            .recover_mut()
                            .set_option_snapshot(option_snapshot.clone());
                        graph
                            .payload
                            .recover_mut()
                            .set_option_snapshot(option_snapshot.clone());
                    }
                    metalink_graphs.extend(graphs);
                }
                #[cfg(feature = "metalink")]
                {
                    let groups = converter
                        .create_resource_groups_from_bytes(data, &options, &mut gid_iter)
                        .map_err(|e| format!("Metalink resource construction failed: {}", e))?;
                    for group in &groups {
                        group
                            .recover_mut()
                            .set_option_snapshot(option_snapshot.clone());
                    }
                    metalink_resource_groups.extend(groups);
                }
            }
        }
        let command_tx = engine.engine_command_sender();
        let mut gids = Vec::new();
        #[cfg(feature = "metalink")]
        let submitted_resource_groups = !metalink_resource_groups.is_empty();
        #[cfg(feature = "metalink")]
        if submitted_resource_groups {
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
            for graph in metalink_graphs {
                let payload_gid = graph.payload.recover().gid();
                command_tx
                    .send(EngineCommand::AddMetalinkGraph { graph })
                    .map_err(|e| format!("Failed to submit Metalink graph: {}", e))?;
                gids.push(payload_gid.value());
            }
            return Ok(gids);
        }

        for (i, input) in self.detected_inputs.iter().enumerate() {
            #[cfg(not(feature = "bittorrent"))]
            if matches!(input.input_type, InputType::MagnetLink) {
                return Err(
                    "BitTorrent support not enabled (compile with --features bittorrent)"
                        .to_string(),
                );
            }
            #[cfg(not(feature = "sftp"))]
            if matches!(input.input_type, InputType::SftpUrl) {
                return Err("SFTP support not enabled (compile with --features sftp)".to_string());
            }
            #[cfg(not(feature = "metalink"))]
            if matches!(input.input_type, InputType::MetalinkFile) {
                return Err(
                    "Metalink support not enabled (compile with --features metalink)".to_string(),
                );
            }
            if matches!(input.input_type, InputType::MetalinkFile) {
                return Err("Metalink inputs must be submitted as a complete set".to_string());
            }

            let gid = if i == 0 {
                explicit_gid.unwrap_or_else(|| self.request_man.next_available_gid())
            } else {
                self.request_man.next_available_gid()
            };
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
            group
                .recover_mut()
                .set_option_snapshot(option_snapshot.clone());
            if options.uses_memory_download() {
                group.recover().mark_in_memory_download();
            }
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
        cmd_tx: aria2_core::engine::engine_command::EngineCommandSender,
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
    /// * `show_progress` - If true, render progress to stdout. TTY output is
    ///   rendered in place; redirected output uses plain flushed lines.
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

            // Create the reporter before the engine starts. This captures the
            // pre-existing stopped-result set without racing a very fast
            // download into the reporter's history filter.
            let (_reporter_stop_tx, reporter_handle) = if show_progress {
                let group_man = self.request_man.clone();
                let summary_interval = self.get_opt_i64("summary-interval").await.unwrap_or(60);
                let output_to_stderr = self.get_opt_bool("stderr").await.unwrap_or(false);
                let (mut reporter, stop_tx) =
                    crate::ui::console_progress::ConsoleProgressReporter::new_with_options(
                        group_man,
                        summary_interval,
                        output_to_stderr,
                    );
                let handle = tokio::spawn(async move {
                    reporter.run().await;
                });
                (Some(stop_tx), Some(handle))
            } else {
                (None, None)
            };

            // Spawn the engine in a background task so the progress reporter
            // can observe its activity concurrently.
            let engine_handle = tokio::spawn(async move { engine.run().await });

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
