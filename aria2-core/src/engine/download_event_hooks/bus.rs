use super::*;
use crate::util::rwlock_ext::RwLockRecover;
use tracing::{debug, info, warn};

impl DownloadEventHooks {
    /// Create a new hook manager with no global hooks and no listeners.
    pub fn new() -> Self {
        let (notifications, _) = broadcast::channel(256);
        Self {
            global_hooks: Arc::new(std::sync::RwLock::new(Vec::new())),
            listeners: std::sync::RwLock::new(Vec::new()),
            next_listener_id: AtomicU64::new(1),
            one_shot: std::sync::RwLock::new(OneShotLedger::default()),
            notifications,
            metadata_history: std::sync::RwLock::new(VecDeque::new()),
        }
    }

    /// Return the process-wide hook bus.
    ///
    /// Download lifecycle transitions happen deep inside individual
    /// `Command` implementations that have no reference to the engine, while
    /// the engine loop remains the central group-management path. A process-wide
    /// bus is what lets a single listener registration
    /// observe *all* of them, and mirrors C++ where `Notifier` is a
    /// `SingletonHolder`. The same pattern is already used in this crate for
    /// `filesystem::file_allocation_man::shared()` and
    /// `checksum::check_integrity::man::shared()`.
    pub fn shared() -> &'static Arc<DownloadEventHooks> {
        SHARED_HOOKS.get_or_init(|| Arc::new(DownloadEventHooks::new()))
    }

    /// Register an observer that is notified of every fired download event.
    ///
    /// Registration is additive. Call [`Self::add_listener_with_id`] when the
    /// caller owns a bounded-lifetime adapter and needs explicit cleanup.
    pub fn add_listener(&self, listener: Arc<dyn DownloadEventListener>) {
        let _ = self.add_listener_with_id(listener);
    }

    /// Register an observer and return its removal identifier.
    ///
    /// The identifier is local to this hook bus and remains valid until the
    /// listener is removed or the bus is dropped. Registration still prunes
    /// listeners whose [`DownloadEventListener::is_alive`] method is false.
    pub fn add_listener_with_id(
        &self,
        listener: Arc<dyn DownloadEventListener>,
    ) -> DownloadEventListenerId {
        let id = self
            .next_listener_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut listeners = self.listeners.recover_mut();
        listeners.retain(|(_, listener)| listener.is_alive());
        listeners.push((id, listener));
        debug!(
            listener_id = id,
            count = listeners.len(),
            "Registered download event listener"
        );
        id
    }

    /// Remove a listener previously returned by [`Self::add_listener_with_id`].
    ///
    /// Returns `true` only when the requested identifier was registered. Any
    /// other listeners that are already dead are pruned as part of the same
    /// operation.
    pub fn remove_listener(&self, id: DownloadEventListenerId) -> bool {
        let mut listeners = self.listeners.recover_mut();
        let removed = listeners.iter().any(|(listener_id, _)| *listener_id == id);
        listeners.retain(|(listener_id, listener)| *listener_id != id && listener.is_alive());
        removed
    }

    /// Number of currently registered observers (primarily for tests).
    pub fn listener_count(&self) -> usize {
        self.listeners.recover().len()
    }

    /// Subscribe to lifecycle and metadata notifications without installing a
    /// callback object. Events are delivered in publication order for this
    /// receiver, subject to the bounded channel's lag semantics.
    pub fn subscribe(&self) -> DownloadEventStream {
        DownloadEventStream {
            receiver: self.notifications.subscribe(),
        }
    }

    /// Return the most recent metadata transition involving `gid`.
    ///
    /// This is used by high-level handles to close the race between creating
    /// an event receiver and checking whether metadata was already resolved.
    pub(crate) fn metadata_event_for(&self, gid: GroupId) -> Option<MetadataResolvedEvent> {
        self.metadata_history
            .recover()
            .iter()
            .rev()
            .find(|event| event.metadata_gid == gid || event.download_gids.contains(&gid))
            .cloned()
    }

    /// Notify every registered observer of `event` for `gid`.
    ///
    /// `gid` must be the canonical 16-digit hex GID
    /// ([`GroupId::to_hex_string`](crate::request::request_group::GroupId::to_hex_string)).
    ///
    /// This is the *unconditional* sink: it is deliberately independent of
    /// whether the user configured an `--on-download-*` shell command, because
    /// RPC clients (AriaNg, webui-aria2, ...) rely on the corresponding
    /// WebSocket notifications to ever mark a download as finished.
    ///
    /// One-shot events are de-duplicated so that a transition observed both at
    /// the `RequestGroup` state change and again at engine-loop demotion is
    /// only delivered once.
    pub fn notify_listeners(&self, event: DownloadEvent, gid: &str) {
        self.notify_listeners_in_scope(event, gid, None);
    }

    /// Notify observers with the owning request manager's identity.
    pub(crate) fn notify_listeners_scoped(&self, event: DownloadEvent, gid: &str, scope_id: u64) {
        self.notify_listeners_in_scope(event, gid, Some(scope_id));
    }

    fn notify_listeners_in_scope(&self, event: DownloadEvent, gid: &str, scope_id: Option<u64>) {
        // Snapshot under a short read lock so a listener callback can never
        // deadlock against `add_listener`.
        let listeners: Vec<Arc<dyn DownloadEventListener>> = {
            let mut guard = self.listeners.recover_mut();
            guard.retain(|(_, listener)| listener.is_alive());
            if guard.is_empty() && self.notifications.receiver_count() == 0 {
                // Nothing to do — and importantly, do not pollute the
                // de-duplication ledger when no one is listening.
                return;
            }
            guard
                .iter()
                .map(|(_, listener)| Arc::clone(listener))
                .collect()
        };

        if event.is_once_per_download()
            && !self
                .one_shot
                .recover_mut()
                .claim((scope_id, gid.to_string(), event))
        {
            debug!(
                event = event.name(),
                gid, "Suppressed duplicate one-shot download event"
            );
            return;
        }

        let _ = self.notifications.send(DownloadNotification::Lifecycle {
            event,
            gid: gid.to_string(),
        });

        debug!(
            event = event.name(),
            gid,
            listeners = listeners.len(),
            "Notifying download event listeners"
        );
        for listener in listeners {
            if let Some(scope_id) = scope_id {
                listener.on_download_event_scoped(event, gid, scope_id);
            } else {
                listener.on_download_event(event, gid);
            }
        }
    }

    /// Notify observers that a metadata task produced its download groups.
    ///
    /// The caller must invoke this after the child groups have been inserted
    /// into the request manager. The event is synchronous and follows the
    /// same non-blocking, non-panicking listener contract as lifecycle events.
    pub fn notify_metadata_resolved(&self, event: MetadataResolvedEvent) {
        {
            let mut history = self.metadata_history.recover_mut();
            history.retain(|previous| previous.metadata_gid != event.metadata_gid);
            history.push_back(event.clone());
            while history.len() > METADATA_HISTORY_CAPACITY {
                history.pop_front();
            }
        }

        let listeners: Vec<Arc<dyn DownloadEventListener>> = {
            let mut guard = self.listeners.recover_mut();
            guard.retain(|(_, listener)| listener.is_alive());
            if guard.is_empty() && self.notifications.receiver_count() == 0 {
                return;
            }
            guard
                .iter()
                .map(|(_, listener)| Arc::clone(listener))
                .collect()
        };

        let _ = self
            .notifications
            .send(DownloadNotification::MetadataResolved(event.clone()));

        debug!(
            metadata_gid = event.metadata_gid.value(),
            download_gids = event.download_gids.len(),
            listeners = listeners.len(),
            "Notifying metadata-resolved listeners"
        );
        for listener in listeners {
            listener.on_metadata_resolved(&event);
        }
    }

    /// Register a global hook for the given event.
    ///
    /// Global hooks are used when no per-group hook is configured.
    /// If a global hook is already registered for this event, it is replaced.
    pub fn set_global_hook(&self, event: DownloadEvent, command: String) {
        let mut hooks = self.global_hooks.recover_mut();
        if let Some(entry) = hooks.iter_mut().find(|(e, _)| *e == event) {
            entry.1 = command;
        } else {
            hooks.push((event, command));
        }
        debug!(
            event = event.name(),
            "Registered global download event hook"
        );
    }

    /// Remove a global hook for the given event.
    pub fn remove_global_hook(&self, event: DownloadEvent) {
        let mut hooks = self.global_hooks.recover_mut();
        hooks.retain(|(e, _)| *e != event);
    }

    /// Fire a download event hook for the given group.
    ///
    /// This method resolves the hook command from (in priority order):
    /// 1. Per-group `DownloadOptions` fields (e.g. `on_download_start`)
    /// 2. Global hooks registered via `set_global_hook()`
    ///
    /// If a command is found, it is spawned as an async child process
    /// with arguments: `<command> <GID_hex> <numFiles> <firstFilePath>`
    ///
    /// This is fire-and-forget — the caller is not blocked.
    /// Errors are logged but do not propagate.
    ///
    /// Mirrors C++ `util::executeHookByOptName(group, option, pref)` followed
    /// by `Notifier::notifyDownloadEvent()` — C++ performs both, in that
    /// order, at every hook site.
    pub fn fire_event(&self, event: DownloadEvent, group: &RequestGroup) {
        // C++ `util::executeHook()` formats the GID with `GroupId::toHex()`,
        // i.e. a zero-padded 16-digit lowercase hex string. Use the same
        // canonical form here so hook arguments match the original *and* so
        // observers receive exactly the GID the JSON-RPC layer publishes.
        let gid_hex = group.gid().to_hex_string();

        // ── Sink 1: user-configured shell hook (optional) ──────────────────
        match self.resolve_hook_command(event, group) {
            Some(command) if !command.is_empty() => {
                let (num_files, first_file_path) = Self::extract_file_info(group);
                self.spawn_hook(&command, &gid_hex, num_files, &first_file_path, event);
            }
            _ => {
                debug!(
                    event = event.name(),
                    gid = %gid_hex,
                    "No hook command configured, skipping shell hook"
                );
            }
        }

        // ── Sink 2: observers (unconditional) ─────────────────────────────
        // MUST run even when no shell hook is configured: the RPC WebSocket
        // notifications (aria2.onDownloadComplete / onDownloadError /
        // onBtDownloadComplete) are delivered through this path, and gating
        // them on an unrelated `--on-download-*` CLI option would leave every
        // default deployment without completion notifications.
        self.notify_listeners_scoped(event, &gid_hex, group.event_scope_id());
    }

    /// Fire a download event hook using direct parameters instead of a group.
    ///
    /// Useful when the RequestGroup is no longer available (e.g. after demotion)
    /// but the relevant data has already been extracted.
    ///
    /// `gid_hex` must be the canonical 16-digit hex GID (as produced by
    /// [`DownloadEventContext::from_group`]).
    pub fn fire_event_with_params(
        &self,
        event: DownloadEvent,
        gid_hex: &str,
        num_files: usize,
        first_file_path: &str,
        command: &str,
    ) {
        self.fire_event_with_params_in_scope(
            event,
            gid_hex,
            num_files,
            first_file_path,
            command,
            None,
        );
    }

    pub(crate) fn fire_event_with_params_scoped(
        &self,
        event: DownloadEvent,
        gid_hex: &str,
        num_files: usize,
        first_file_path: &str,
        command: &str,
        scope_id: u64,
    ) {
        self.fire_event_with_params_in_scope(
            event,
            gid_hex,
            num_files,
            first_file_path,
            command,
            Some(scope_id),
        );
    }

    fn fire_event_with_params_in_scope(
        &self,
        event: DownloadEvent,
        gid_hex: &str,
        num_files: usize,
        first_file_path: &str,
        command: &str,
        scope_id: Option<u64>,
    ) {
        // Sink 1 — only when a shell command was actually configured.
        if !command.is_empty() {
            self.spawn_hook(command, gid_hex, num_files, first_file_path, event);
        }
        // Sink 2 — always. See `fire_event` for why this must not be gated.
        self.notify_listeners_in_scope(event, gid_hex, scope_id);
    }

    /// Extract `(numFiles, firstFilePath)` hook arguments from a group.
    ///
    /// Mirrors the C++ `executeHookByOptName` body, which reads
    /// `group->getDownloadContext()->getNumFileEntries()` and the first
    /// requested file entry's path.
    fn extract_file_info(group: &RequestGroup) -> (usize, String) {
        if let Some(dctx) = group.download_context.recover().as_ref() {
            let num = dctx.count_requested_file_entry();
            let path = dctx.first_file_path().unwrap_or_default().to_string();
            (num, path)
        } else {
            (0, String::new())
        }
    }

    /// Resolve the hook command for an event from per-group or global config.
    fn resolve_hook_command(&self, event: DownloadEvent, group: &RequestGroup) -> Option<String> {
        // Priority 1: per-group options
        let opts = group.options_arc();
        let per_group = match event {
            DownloadEvent::Start => opts.on_download_start.as_ref(),
            DownloadEvent::Complete => opts.on_download_complete.as_ref(),
            DownloadEvent::Error => opts.on_download_error.as_ref(),
            DownloadEvent::Pause => opts.on_download_pause.as_ref(),
            DownloadEvent::Stop => opts.on_download_stop.as_ref(),
            DownloadEvent::BtComplete => opts.on_bt_download_complete.as_ref(),
        };

        if let Some(cmd) = per_group
            && !cmd.is_empty()
        {
            return Some(cmd.clone());
        }

        // Priority 2: global hooks
        let hooks = self.global_hooks.recover();
        for (e, cmd) in hooks.iter() {
            if *e == event && !cmd.is_empty() {
                return Some(cmd.clone());
            }
        }

        None
    }

    /// Spawn the hook command as an async child process.
    ///
    /// Mirrors C++ `util::executeHook()` which forks on Unix or
    /// `CreateProcessW` on Windows. The Rust version uses
    /// `tokio::process::Command` which handles both platforms.
    fn spawn_hook(
        &self,
        command: &str,
        gid_hex: &str,
        num_files: usize,
        first_file_path: &str,
        event: DownloadEvent,
    ) {
        info!(
            event = event.name(),
            command,
            gid = gid_hex,
            num_files,
            path = first_file_path,
            "Executing download event hook"
        );

        // Split command into program + args for proper process spawning.
        // C++ uses execlp(command, command, gidStr, numFilesStr, firstFilename, NULL)
        // which searches PATH and passes the arguments directly.
        let num_files_str = num_files.to_string();

        // On Windows, handle .bat/.cmd files specially (like C++ does).
        #[cfg(target_os = "windows")]
        let result = {
            let is_batch = command.to_lowercase().ends_with(".bat")
                || command.to_lowercase().ends_with(".cmd");

            if is_batch {
                let cmd_exe = std::env::var("windir")
                    .map(|w| format!("{}\\system32\\cmd.exe", w))
                    .unwrap_or_else(|_| "cmd.exe".to_string());

                let mut cmd = tokio::process::Command::new(&cmd_exe);
                cmd.args(["/c", command, gid_hex, &num_files_str, first_file_path])
                    .creation_flags(0x08000000); // CREATE_NO_WINDOW
                cmd.spawn()
            } else {
                tokio::process::Command::new(command)
                    .args([gid_hex, &num_files_str, first_file_path])
                    .spawn()
            }
        };

        #[cfg(not(target_os = "windows"))]
        let result = {
            tokio::process::Command::new(command)
                .args([gid_hex, &num_files_str, first_file_path])
                .spawn()
        };

        match result {
            Ok(mut child) => {
                if let Some(id) = child.id() {
                    debug!(event = event.name(), pid = id, "Hook process spawned");
                }
                // Fire-and-forget: await the child in a detached task
                // to reap zombies on Unix.
                let event_name = event.name().to_string();
                tokio::spawn(async move {
                    match child.wait().await {
                        Ok(status) => {
                            if !status.success() {
                                warn!(
                                    event = %event_name,
                                    exit = ?status.code(),
                                    "Hook process exited with non-zero status"
                                );
                            }
                        }
                        Err(e) => {
                            warn!(
                                event = %event_name,
                                error = %e,
                                "Failed to wait for hook process"
                            );
                        }
                    }
                });
            }
            Err(e) => {
                warn!(
                    event = event.name(),
                    command,
                    error = %e,
                    "Failed to spawn hook process"
                );
            }
        }
    }
}

impl Default for DownloadEventHooks {
    fn default() -> Self {
        Self::new()
    }
}
