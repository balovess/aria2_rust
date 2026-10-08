use super::*;

impl Aria2RustSession {
    pub(super) fn new(raw_options: Vec<(String, String)>) -> std::result::Result<Self, String> {
        let runtime = Runtime::new().map_err(|error| format!("runtime init failed: {error}"))?;
        let (config, keep_running) = runtime.block_on(async {
            let mut config = ConfigManager::new();
            let mut keep_running = false;
            for (name, value) in raw_options {
                if name == "keep-running" {
                    keep_running = parse_bool(&value)?;
                    continue;
                }
                // aria2's C++ API ignores unknown options passed to
                // sessionNew. Known options still go through the registry so
                // type/range errors are reported instead of silently changing
                // the effective configuration.
                if !config.registry().contains(&name) {
                    continue;
                }
                config
                    .set_global_option(&name, OptionValue::Str(value))
                    .await
                    .map_err(|error| format!("invalid option {name}: {error}"))?;
            }
            Ok::<_, String>((config, keep_running))
        })?;

        let request_man = Arc::new(RequestGroupMan::new());
        let mut engine = DownloadEngine::new();
        #[cfg(feature = "bittorrent")]
        {
            let sources = runtime.block_on(config.get_global_option("bt-tracker-source"));
            let sources = match sources {
                Some(OptionValue::List(values)) => values,
                Some(OptionValue::Str(value)) => value
                    .split([',', '\n'])
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .collect(),
                _ => Vec::new(),
            };
            let update_interval = runtime
                .block_on(config.get_global_i64("bt-tracker-update-interval"))
                .filter(|seconds| *seconds > 0)
                .map(|seconds| Duration::from_secs(seconds as u64))
                .unwrap_or(
                    aria2_protocol::bittorrent::tracker::public_list::DEFAULT_TRACKER_UPDATE_INTERVAL,
                );
            let enabled = runtime
                .block_on(config.get_global_bool("enable-public-trackers"))
                .unwrap_or(true);
            engine.set_public_tracker_config(
                aria2_protocol::bittorrent::tracker::public_list::TrackerCatalogConfig {
                    enabled,
                    sources,
                    update_interval,
                },
            );
        }
        engine.set_request_group_man(Arc::clone(&request_man));
        // The C API keeps the event loop alive between synchronous `run` calls,
        // matching the original library's RUN_ONCE mode.
        engine.set_keep_alive(true);

        let max_concurrent = runtime.block_on(config.get_global_i64("max-concurrent-downloads"));
        if let Some(max) = max_concurrent.filter(|value| *value >= 0) {
            request_man.set_max_concurrent(max as u32);
        }

        let download_limit = runtime
            .block_on(config.get_global_i64("max-overall-download-limit"))
            .and_then(non_zero_limit);
        let upload_limit = runtime
            .block_on(config.get_global_i64("max-overall-upload-limit"))
            .and_then(non_zero_limit);
        if download_limit.is_some() || upload_limit.is_some() {
            engine.set_global_rate_limiter(RateLimiterConfig::new(download_limit, upload_limit));
            request_man.set_global_speed_limit(download_limit, upload_limit);
        }

        let command_tx = engine.engine_command_sender();
        let shutdown_tx = engine.take_shutdown_sender();
        let engine_task = runtime.spawn(engine.run());

        Ok(Self {
            runtime,
            config,
            request_man,
            command_tx,
            shutdown_tx,
            engine_task: Some(engine_task),
            keep_running,
            last_error: String::new(),
            download_event_callback: None,
        })
    }

    pub(super) fn fail<T>(&mut self, message: impl Into<String>, fallback: T) -> T {
        self.last_error = message.into();
        fallback
    }

    pub(super) fn merged_options(
        &mut self,
        overrides: Vec<(String, String)>,
    ) -> std::result::Result<DownloadOptions, String> {
        let values = self.runtime.block_on(self.config.get_all_global_options());
        let mut strings = values
            .into_iter()
            .filter_map(|(name, value)| {
                if value.is_none() {
                    None
                } else {
                    Some((name, value.to_string()))
                }
            })
            .collect::<HashMap<_, _>>();

        for (name, value) in overrides {
            if name == "keep-running" {
                continue;
            }
            let Some(definition) = self.config.registry().get(&name) else {
                continue;
            };
            let parsed = definition
                .parse_value(&value)
                .map_err(|error| format!("invalid option {name}: {error}"))?;
            strings.insert(name, parsed.to_string());
        }
        Ok(DownloadOptions::from_option_strings(&strings))
    }

    pub(super) fn add_uri(
        &mut self,
        uris: Vec<String>,
        overrides: Vec<(String, String)>,
    ) -> std::result::Result<u64, String> {
        if uris.is_empty() || uris.iter().any(String::is_empty) {
            return Err("at least one non-empty URI is required".to_string());
        }
        let options = self.merged_options(overrides)?;
        let gid = {
            let man = &self.request_man;
            let gid = man.next_available_gid();
            let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
                gid,
                uris,
                options.clone(),
            )));
            if options.pause {
                group
                    .recover_mut()
                    .pause()
                    .map_err(|error| error.to_string())?;
            }
            man.add_group_arc(Arc::clone(&group));
            if let Err(error) = self.command_tx.send(EngineCommand::AddDownload { group }) {
                let _ = man.remove_group_by_id(gid);
                return Err(error.to_string());
            }
            Ok::<_, String>(gid.value())
        }?;
        Ok(gid)
    }

    #[cfg(feature = "bittorrent")]
    pub(super) fn add_torrent(
        &mut self,
        data: Vec<u8>,
        web_seed_uris: Vec<String>,
        overrides: Vec<(String, String)>,
    ) -> std::result::Result<u64, String> {
        if data.is_empty() {
            return Err("torrent data must not be empty".to_string());
        }
        let options = self.merged_options(overrides)?;
        let gid = self.request_man.next_available_gid();
        let mut uris = Vec::with_capacity(1);
        uris.push(format!("bt://{}", gid.to_hex_string()));
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid,
            uris,
            options.clone(),
        )));
        if options.pause {
            group
                .recover_mut()
                .pause()
                .map_err(|error| error.to_string())?;
        }
        crate::engine::bittorrent::download::command::prepare_group_metadata(
            Arc::clone(&group),
            &data,
            &options,
            options.dir.as_deref(),
            &web_seed_uris,
        )
        .map_err(|error| error.to_string())?;
        group.recover().set_bt_metadata_data(data);
        self.request_man.add_group_arc(Arc::clone(&group));
        if let Err(error) = self.command_tx.send(EngineCommand::AddDownload { group }) {
            let _ = self.request_man.remove_group_by_id(gid);
            return Err(error.to_string());
        }
        Ok(gid.value())
    }

    #[cfg(not(feature = "bittorrent"))]
    pub(super) fn add_torrent(
        &mut self,
        data: Vec<u8>,
        web_seed_uris: Vec<String>,
        overrides: Vec<(String, String)>,
    ) -> std::result::Result<u64, String> {
        let _ = (data, web_seed_uris, overrides);
        Err("BitTorrent is not enabled".to_string())
    }

    #[cfg(feature = "metalink")]
    pub(super) fn add_metalink(
        &mut self,
        data: Vec<u8>,
        overrides: Vec<(String, String)>,
        gid_capacity: Option<usize>,
    ) -> std::result::Result<Vec<u64>, String> {
        if data.is_empty() {
            return Err("metalink data must not be empty".to_string());
        }
        let options = self.merged_options(overrides)?;
        let converter = MetalinkToRequestGroup::new();
        let mut gids = std::iter::from_fn(|| Some(self.request_man.next_available_gid()));
        let expansion = converter
            .create_groups_from_bytes(&data, &options, &mut gids)
            .map_err(|error| error.to_string())?;
        let resource_groups = expansion.resource_groups;
        #[cfg(feature = "bittorrent")]
        let graphs = expansion.torrent_graphs;

        #[cfg(feature = "bittorrent")]
        let required_gids = resource_groups.len() + graphs.len().saturating_mul(2);
        #[cfg(not(feature = "bittorrent"))]
        let required_gids = resource_groups.len();
        if gid_capacity.is_some_and(|capacity| capacity < required_gids) {
            return Err(format!(
                "output buffer too small; required {required_gids} GIDs"
            ));
        }

        let mut response_gids = Vec::new();
        for group in resource_groups {
            let gid = group.recover().gid();
            self.request_man.add_group_arc(Arc::clone(&group));
            if let Err(error) = self.command_tx.send(EngineCommand::AddDownload { group }) {
                let _ = self.request_man.remove_group_by_id(gid);
                return Err(error.to_string());
            }
            response_gids.push(gid.value());
        }

        #[cfg(feature = "bittorrent")]
        {
            for graph in graphs {
                let metadata_gid = graph.metadata.recover().gid();
                let payload_gid = graph.payload.recover().gid();
                let metadata_group = Arc::clone(&graph.metadata);
                let payload_group = Arc::clone(&graph.payload);
                self.request_man
                    .add_metalink_graph(graph)
                    .map_err(|error| error.to_string())?;
                self.command_tx
                    .send(EngineCommand::AddDownload {
                        group: metadata_group,
                    })
                    .map_err(|error| error.to_string())?;
                self.command_tx
                    .send(EngineCommand::AddDownload {
                        group: payload_group,
                    })
                    .map_err(|error| error.to_string())?;
                response_gids.extend([metadata_gid.value(), payload_gid.value()]);
            }
        }

        Ok(response_gids)
    }

    #[cfg(not(feature = "metalink"))]
    pub(super) fn add_metalink(
        &mut self,
        data: Vec<u8>,
        overrides: Vec<(String, String)>,
        gid_capacity: Option<usize>,
    ) -> std::result::Result<Vec<u64>, String> {
        let _ = (data, overrides, gid_capacity);
        Err("Metalink is not enabled".to_string())
    }

    pub(super) fn run(&mut self, mode: u32) -> i32 {
        let keep_running = self.keep_running;
        let request_man = Arc::clone(&self.request_man);
        self.runtime.block_on(async {
            if mode == 1 {
                if !keep_running && request_man.download_finished() {
                    return 0;
                }
                tokio::task::yield_now().await;
                return if keep_running || !request_man.download_finished() {
                    1
                } else {
                    0
                };
            }

            let notifier = request_man.download_finished_notifier();
            loop {
                let notified = notifier.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if request_man.download_finished() {
                    return 0;
                }
                notified.await;
            }
        })
    }

    pub(super) fn wait_download(
        &self,
        gid: u64,
        timeout_ms: u64,
    ) -> std::result::Result<Aria2RustDownloadInfo, i32> {
        let request_man = Arc::clone(&self.request_man);
        let wait = async move {
            let signal = request_man.activity_signal();
            let mut observed = signal.generation();
            loop {
                let Some(info) = download_info_from_manager(&request_man, gid) else {
                    return Err(INVALID_ARGUMENT);
                };
                if matches!(
                    info.status,
                    status if status == Aria2RustDownloadStatus::Complete as u32
                        || status == Aria2RustDownloadStatus::Error as u32
                        || status == Aria2RustDownloadStatus::Removed as u32
                ) {
                    return Ok(info);
                }
                signal.wait_for_change(&mut observed).await;
            }
        };

        if timeout_ms == 0 {
            self.runtime.block_on(wait)
        } else {
            self.runtime.block_on(async {
                tokio::time::timeout(Duration::from_millis(timeout_ms), wait)
                    .await
                    .unwrap_or(Err(TIMEOUT))
            })
        }
    }

    pub(super) fn finalize(&mut self) -> i32 {
        let _ = self.command_tx.send(EngineCommand::ForceHaltAll {
            reason: HaltReason::ShutdownSignal,
        });
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        if let Some(engine_task) = self.engine_task.take() {
            let _ = self.runtime.block_on(engine_task);
        }
        0
    }

    pub(super) fn change_global_options(&mut self, options: Vec<(String, String)>) -> i32 {
        let mut download_limit_changed = false;
        let mut upload_limit_changed = false;
        let mut max_concurrent = None;
        #[cfg(feature = "bittorrent")]
        let mut public_tracker_sources = None;
        #[cfg(feature = "bittorrent")]
        let mut public_tracker_update_interval = None;
        #[cfg(feature = "bittorrent")]
        let mut public_trackers_enabled = None;
        for (name, value) in options {
            if name == "keep-running" {
                match parse_bool(&value) {
                    Ok(value) => self.keep_running = value,
                    Err(error) => return self.fail(error, INVALID_ARGUMENT),
                }
                continue;
            }
            if !self.config.registry().contains(&name) {
                continue;
            }
            let parsed = match self
                .config
                .registry()
                .get(&name)
                .and_then(|def| def.parse_value(&value).ok())
            {
                Some(value) => value,
                None => return self.fail(format!("invalid option {name}"), INVALID_ARGUMENT),
            };
            if self
                .runtime
                .block_on(
                    self.config
                        .set_global_option(&name, OptionValue::Str(value.clone())),
                )
                .is_err()
            {
                return self.fail(format!("invalid option {name}"), INVALID_ARGUMENT);
            }
            match name.as_str() {
                "max-concurrent-downloads" => max_concurrent = parsed.as_i64(),
                "max-overall-download-limit" => download_limit_changed = true,
                "max-overall-upload-limit" => upload_limit_changed = true,
                #[cfg(feature = "bittorrent")]
                "bt-tracker-source" => public_tracker_sources = Some(value),
                #[cfg(feature = "bittorrent")]
                "bt-tracker-update-interval" => public_tracker_update_interval = parsed.as_i64(),
                #[cfg(feature = "bittorrent")]
                "enable-public-trackers" => public_trackers_enabled = parsed.as_bool(),
                _ => {}
            }
        }

        if let Some(max) = max_concurrent.filter(|value| *value >= 0) {
            let _ = self
                .command_tx
                .send(EngineCommand::SetMaxConcurrent { max: max as u32 });
        }
        if download_limit_changed || upload_limit_changed {
            let download_limit = self
                .runtime
                .block_on(self.config.get_global_i64("max-overall-download-limit"))
                .and_then(non_zero_limit);
            let upload_limit = self
                .runtime
                .block_on(self.config.get_global_i64("max-overall-upload-limit"))
                .and_then(non_zero_limit);
            let _ = self.command_tx.send(EngineCommand::SetGlobalRateLimit {
                download_limit,
                upload_limit,
            });
        }
        #[cfg(feature = "bittorrent")]
        {
            if let Some(sources) = public_tracker_sources {
                let _ = self
                    .command_tx
                    .send(EngineCommand::SetPublicTrackerSources { sources });
            }
            if let Some(seconds) = public_tracker_update_interval.filter(|seconds| *seconds > 0) {
                let _ = self
                    .command_tx
                    .send(EngineCommand::SetPublicTrackerUpdateInterval {
                        seconds: seconds as u64,
                    });
            }
            if let Some(enabled) = public_trackers_enabled {
                let _ = self
                    .command_tx
                    .send(EngineCommand::SetPublicTrackersEnabled { enabled });
            }
        }
        0
    }

    pub(super) fn change_options(&mut self, gid: u64, options: Vec<(String, String)>) -> i32 {
        let changes = options
            .into_iter()
            .map(|(name, value)| (name, serde_json::Value::String(value)))
            .collect();
        let result = {
            let manager = &self.request_man;
            manager.change_group_options(&GroupId::new(gid).to_hex_string(), changes)
        };
        match result {
            Ok(()) => 0,
            Err(error) => self.fail(error, INVALID_ARGUMENT),
        }
    }

    pub(super) fn pause_all(&mut self, force: bool) -> i32 {
        if force {
            self.request_man.force_pause_all();
        } else {
            self.request_man.pause_all();
        }
        let command = if force {
            EngineCommand::ForcePauseAll
        } else {
            EngineCommand::PauseAll
        };
        self.command_tx
            .send(command)
            .map(|_| 0)
            .unwrap_or_else(|error| self.fail(error.to_string(), INTERNAL_ERROR))
    }

    pub(super) fn unpause_all(&mut self) -> i32 {
        self.request_man.unpause_all();
        self.command_tx
            .send(EngineCommand::UnpauseAll)
            .map(|_| 0)
            .unwrap_or_else(|error| self.fail(error.to_string(), INTERNAL_ERROR))
    }

    pub(super) fn change_position(
        &mut self,
        gid: u64,
        position: i32,
        mode: u32,
    ) -> std::result::Result<usize, String> {
        let mode = match mode {
            0 => PositionMode::SetFromStart,
            1 => PositionMode::MoveFromStart,
            2 => PositionMode::SetFromEnd,
            _ => return Err("invalid queue position mode".to_string()),
        };
        self.request_man
            .change_position(GroupId::new(gid), position, mode)
            .map_err(|error| error.to_string())
    }

    pub(super) fn get_info(&self, gid: u64) -> Option<Aria2RustDownloadInfo> {
        download_info_from_manager(&self.request_man, gid)
    }

    pub(super) fn file_entries(
        &self,
        gid: u64,
    ) -> Option<Vec<crate::request::request_group::FileEntry>> {
        if let Some(group) = self.request_man.find_group(GroupId::new(gid)) {
            return Some(group.recover().create_download_result().files);
        }
        self.request_man
            .find_stopped_result(&GroupId::new(gid).to_hex_string())
            .map(|result| result.files)
    }

    pub(super) fn global_stat(&mut self) -> Aria2RustGlobalStat {
        let manager = &self.request_man;
        let mut stat = Aria2RustGlobalStat {
            num_stopped: manager.stopped_results_len() as u64,
            ..Default::default()
        };
        for group in manager.list_groups() {
            let group = group.recover();
            match group.status() {
                DownloadStatus::Active => {
                    stat.num_active += 1;
                    stat.download_speed =
                        stat.download_speed.saturating_add(group.download_speed());
                    stat.upload_speed = stat.upload_speed.saturating_add(group.upload_speed());
                }
                DownloadStatus::Waiting | DownloadStatus::Paused => stat.num_waiting += 1,
                DownloadStatus::Complete | DownloadStatus::Error(_) | DownloadStatus::Removed => {}
            }
        }
        stat
    }
}
