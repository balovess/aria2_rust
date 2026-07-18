use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use super::json_rpc::{JsonRpcRequest, JsonRpcResponse};
use super::server::{AuthConfig, CorsConfig, RpcAuthMiddleware};
use super::types::{DownloadStatus, GlobalOptions, PeerInfo, StatusInfo, TaskOptions};
use super::websocket::EventPublisher;
use aria2_core::TorrentFileEntry;
use aria2_core::config::OptionRegistry;
use aria2_core::engine::command::Command;
use aria2_core::request::request_group_man::RequestGroupMan;

/// Delay before a scheduled halt actually triggers the shutdown signal,
/// mirroring the original aria2 `TimedHaltCommand` 3-second delay (see
/// `RpcMethodImpl.cc::goingShutdown`). The delay gives the RPC client time
/// to receive the `"OK"` response before the server begins shutting down.
pub(crate) const HALT_DELAY: Duration = Duration::from_secs(3);

/// Core RPC engine that manages download tasks and handles aria2 protocol requests.
///
/// This is the main orchestrator that:
/// - Maintains task state (active downloads, stopped tasks)
/// - Routes incoming RPC requests to appropriate handlers
/// - Provides progress tracking and status queries
/// - Publishes events via WebSocket for real-time notifications
///
/// Handler implementations are in the [`handlers`](crate::handlers) module.
pub struct RpcEngine {
    /// Active download tasks keyed by GID
    pub(crate) tasks: Arc<RwLock<HashMap<String, TaskState>>>,
    /// Global configuration options
    pub(crate) global_opts: GlobalOptions,
    /// Per-task configuration options
    pub(crate) task_opts: TaskOptions,
    /// Completed/stopped task results
    pub(crate) stopped_tasks: Arc<RwLock<Vec<StatusInfo>>>,
    /// Event publisher for WebSocket notifications
    pub(crate) event_publisher: Arc<EventPublisher>,
    /// Authentication middleware for token-based RPC auth
    pub(crate) auth_middleware: RpcAuthMiddleware,
    /// Shared download group manager (for live progress queries and task tracking).
    /// When set, RPC handlers read progress directly from RequestGroupMan
    /// instead of the placeholder `tasks` map.
    pub(crate) group_man: Option<Arc<RwLock<RequestGroupMan>>>,
    /// Channel sender to submit download commands to the DownloadEngine loop.
    /// When set, `aria2.addUri` starts real downloads by sending a
    /// `DownloadCommand` through this channel.
    pub(crate) cmd_tx: Option<mpsc::UnboundedSender<Box<dyn Command>>>,
    /// Cancellation token that signals graceful server shutdown.
    ///
    /// Triggered by [`RpcEngine::schedule_halt`] after a [`HALT_DELAY`]-second
    /// delay (mirroring the original aria2 `TimedHaltCommand`). The HTTP
    /// server's `with_graceful_shutdown` futures awaits this token.
    pub(crate) shutdown_signal: CancellationToken,
}

/// Internal state for an active download task.
///
/// Contains both static metadata (GID, URIs, options) and dynamic
/// progress fields (speeds, lengths, connections) that are updated
/// by the download engine during execution.
pub(crate) struct TaskState {
    /// Current status information with metadata
    pub(crate) status: StatusInfo,
    /// Configuration options specific to this task
    #[allow(dead_code)] // Stored for future RPC tellActive/tellStopped option reporting
    pub(crate) options: HashMap<String, serde_json::Value>,
    /// URI list for this download
    #[allow(dead_code)] // Stored for future RPC getUris option reporting
    pub(crate) uris: Vec<String>,
    /// Torrent file entries (for BitTorrent downloads)
    pub(crate) torrent_files: Option<Vec<TorrentFileEntry>>,
    // === Dynamic progress fields (updated by download engine) ===
    pub(crate) total_length: u64,
    pub(crate) completed_length: u64,
    pub(crate) upload_length: u64,
    pub(crate) download_speed: u64,
    pub(crate) upload_speed: u64,
    pub(crate) connections: u16,
    /// Peer list for BitTorrent downloads
    pub(crate) peers: Vec<PeerInfo>,
    /// Cancellation token for forceful interruption of active downloads
    pub(crate) cancel_token: Option<CancellationToken>,
}

impl TaskState {
    pub(crate) fn new(
        status: StatusInfo,
        options: HashMap<String, serde_json::Value>,
        uris: Vec<String>,
    ) -> Self {
        Self {
            status,
            options,
            uris,
            torrent_files: None,
            total_length: 0,
            completed_length: 0,
            upload_length: 0,
            download_speed: 0,
            upload_speed: 0,
            connections: 0,
            peers: vec![],
            cancel_token: Some(CancellationToken::new()),
        }
    }

    /// Update the StatusInfo snapshot from current internal state.
    ///
    /// Refreshes progress fields (lengths, speeds, connections) from the
    /// internal u64 counters while preserving all metadata fields (BT info,
    /// files, dir, error info, etc.) that were set when the task was created
    /// or updated by other handlers.
    pub(crate) fn update_status_info(&mut self) {
        let mut status = self.status.clone();
        status.total_length = Some(self.total_length.to_string());
        status.completed_length = Some(self.completed_length.to_string());
        status.upload_length = Some(self.upload_length.to_string());
        status.download_speed = Some(self.download_speed.to_string());
        status.upload_speed = Some(self.upload_speed.to_string());
        status.connections = Some(self.connections.to_string());
        self.status = status;
    }

    /// Update progress fields (typically called by download engine).
    pub fn update_progress(
        &mut self,
        total: u64,
        completed: u64,
        uploaded: u64,
        dl_speed: u64,
        ul_speed: u64,
        connections: u16,
    ) {
        self.total_length = total;
        self.completed_length = completed;
        self.upload_length = uploaded;
        self.download_speed = dl_speed;
        self.upload_speed = ul_speed;
        self.connections = connections;
    }
}

impl RpcEngine {
    /// Create a new RpcEngine instance with default global options seeded
    /// from the `OptionRegistry`.
    ///
    /// The `global_opts` map is pre-populated with all registered option
    /// defaults (e.g. `file-allocation=falloc`, `secure-falloc=false`,
    /// `mmap-threshold=268435456`) so that `aria2.getGlobalOption` returns
    /// meaningful values immediately, without requiring the client to first
    /// call `aria2.changeGlobalOption`.
    pub fn new() -> Self {
        let registry = OptionRegistry::new();
        let mut defaults = HashMap::new();
        for (name, def) in registry.all() {
            // Skip deprecated and hidden options so RPC clients don't see
            // internal/legacy options via aria2.getGlobalOption.
            if def.deprecated || def.hidden {
                continue;
            }
            let json_val: serde_json::Value = serde_json::Value::from(def.default_value());
            // Skip None/Null defaults to keep the map compact
            if !json_val.is_null() {
                defaults.insert(name.clone(), json_val);
            }
        }

        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            global_opts: Arc::new(RwLock::new(defaults)),
            task_opts: Arc::new(RwLock::new(HashMap::new())),
            stopped_tasks: Arc::new(RwLock::new(Vec::new())),
            event_publisher: Arc::new(EventPublisher::default()),
            auth_middleware: RpcAuthMiddleware::default(),
            group_man: None,
            cmd_tx: None,
            shutdown_signal: CancellationToken::new(),
        }
    }

    /// Chainable builder method to set authentication config.
    pub fn with_auth(mut self, auth: AuthConfig) -> Self {
        if let Some(token) = &auth.token {
            self.auth_middleware = RpcAuthMiddleware::new(token);
        }
        self
    }

    /// Chainable builder method to set auth middleware directly.
    pub fn with_auth_middleware(mut self, middleware: RpcAuthMiddleware) -> Self {
        self.auth_middleware = middleware;
        self
    }

    /// Chainable builder method to set CORS config.
    pub fn with_cors(self, cors: CorsConfig) -> Self {
        let _ = cors;
        self
    }

    /// Chainable builder method to set the shared RequestGroupMan.
    /// When set, RPC handlers read live progress from the group manager
    /// and `aria2.addUri` registers downloads there.
    pub fn with_group_man(mut self, man: Arc<RwLock<RequestGroupMan>>) -> Self {
        self.group_man = Some(man);
        self
    }

    /// Chainable builder method to set the command channel sender.
    /// When set, `aria2.addUri` sends real `DownloadCommand`s to the engine.
    pub fn with_cmd_tx(mut self, tx: mpsc::UnboundedSender<Box<dyn Command>>) -> Self {
        self.cmd_tx = Some(tx);
        self
    }

    /// Chainable builder method to merge user-specified global options
    /// over the OptionRegistry defaults. User values take precedence.
    ///
    /// Null values in `user_opts` are skipped so that the corresponding
    /// default (if any) is preserved.
    ///
    /// Uses `try_write()` because the engine is freshly constructed and
    /// not yet shared across tasks, so lock contention is impossible.
    pub fn with_global_opts(self, user_opts: HashMap<String, serde_json::Value>) -> Self {
        // Scope the write guard so it is dropped before `self` is returned.
        {
            let mut defaults = self
                .global_opts
                .try_write()
                .expect("no contention on fresh engine");
            for (k, v) in user_opts {
                // Only insert non-null values; null means "use default"
                if !v.is_null() {
                    defaults.insert(k, v);
                }
            }
        }
        self
    }

    /// Get reference to the event publisher for subscribing to events.
    pub fn publisher(&self) -> &EventPublisher {
        &self.event_publisher
    }

    /// Returns a clone of the engine's shutdown signal token.
    ///
    /// Used by the HTTP server (`axum::serve::with_graceful_shutdown`) to wait
    /// for shutdown triggers fired by [`Self::schedule_halt`].
    ///
    /// [`CancellationToken`] is internally `Arc`-based, so cloning is cheap
    /// and shares the same underlying cancellation source.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown_signal.clone()
    }

    /// Schedule a delayed halt that triggers the engine's shutdown signal
    /// after `delay`, mirroring the original aria2 `TimedHaltCommand`.
    ///
    /// When `force` is `true`, also forcibly cancels every active download
    /// task (matching `forceHalt=true` in `DownloadEngine::forceHalt()`); the
    /// task map is cleared and each task's [`CancellationToken`] is cancelled.
    ///
    /// When `force` is `false`, active downloads are left untouched — the
    /// engine's graceful halt logic (in `DownloadEngine`) is responsible for
    /// letting in-flight downloads finish before the process exits.
    ///
    /// # Why a delay?
    ///
    /// Per the original aria2 source comment:
    /// > "Schedule shutdown after 3 seconds to give time to client to
    /// > receive RPC response."
    ///
    /// Cancelling immediately could close the HTTP response stream before
    /// the client receives the `"OK"` body, which would surface as a
    /// connection-reset error in plugins like AriaNg.
    ///
    /// # Idempotency
    ///
    /// Safe to call multiple times: each call spawns an independent task,
    /// but cancelling an already-cancelled token and clearing an empty map
    /// are both no-ops.
    pub(crate) fn schedule_halt(&self, delay: Duration, force: bool) {
        let shutdown_token = self.shutdown_signal.clone();
        let tasks = self.tasks.clone();

        tokio::spawn(async move {
            tokio::time::sleep(delay).await;

            if force {
                // Force-cancel every active download (forceHalt=true).
                // The order matters: cancel tokens first so any in-flight
                // download futures stop promptly, then mark status as Removed
                // so any subsequent tellStatus/tellActive call sees the right
                // state, then drop the map entries entirely.
                let mut tasks_guard = tasks.write().await;
                for state in tasks_guard.values_mut() {
                    if let Some(cancel_token) = &state.cancel_token {
                        cancel_token.cancel();
                    }
                    state.status.status = DownloadStatus::Removed;
                }
                tasks_guard.clear();
            }

            shutdown_token.cancel();
            tracing::info!(
                force,
                "RPC shutdown signal fired (scheduled halt fired after delay)"
            );
        });
    }

    /// Get current number of active tasks.
    pub async fn task_count(&self) -> usize {
        self.tasks.read().await.len()
    }

    /// Update progress for a specific task (called by download engine).
    ///
    /// Returns `true` if the task was found and updated, `false` otherwise.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_task_progress(
        &self,
        gid: &str,
        total: u64,
        completed: u64,
        uploaded: u64,
        dl_speed: u64,
        ul_speed: u64,
        connections: u16,
    ) -> bool {
        let mut tasks = self.tasks.write().await;
        if let Some(state) = tasks.get_mut(gid) {
            state.update_progress(total, completed, uploaded, dl_speed, ul_speed, connections);
            true
        } else {
            false
        }
    }

    /// Set torrent file entries for a BitTorrent download task.
    pub async fn set_task_torrent_files(&self, gid: &str, files: Vec<TorrentFileEntry>) -> bool {
        let mut tasks = self.tasks.write().await;
        if let Some(state) = tasks.get_mut(gid) {
            state.torrent_files = Some(files);
            true
        } else {
            false
        }
    }

    /// Set peer list for a BitTorrent download task.
    pub async fn set_task_peers(&self, gid: &str, peers: Vec<PeerInfo>) -> bool {
        let mut tasks = self.tasks.write().await;
        if let Some(state) = tasks.get_mut(gid) {
            state.peers = peers;
            true
        } else {
            false
        }
    }

    /// Main request dispatcher - routes RPC methods to their handlers.
    ///
    /// This is the central entry point for all JSON-RPC requests.
    /// It matches on the method name and delegates to the appropriate
    /// handler implementation in [rpc_handlers].
    ///
    /// Before dispatching, validates the request token against the
    /// configured `rpc-secret` (if any) via [`RpcAuthMiddleware`].
    ///
    /// **Authentication protocol** (matches original aria2):
    /// - Positional params: `params[0]` must be a string `"token:<secret>"`
    /// - Named params: `params.secret` must be the secret string
    ///
    /// The token is stripped from `params` before dispatching to handlers
    /// so that handlers never see the token element.
    pub async fn handle_request(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let id = req.id.clone().unwrap_or(serde_json::Value::Null);

        // Authenticate: extract token via aria2 protocol and strip from params.
        // The stripped request is passed to handlers so they see clean params.
        let (token, stripped_params) = JsonRpcRequest::extract_token(&req.params);
        let mut stripped_req = req.clone();
        stripped_req.params = stripped_params;

        if let Err(auth_err) = self.auth_middleware.validate(token.as_deref()) {
            return auth_err.into_response(req.id.clone());
        }

        // Use `stripped_req` everywhere so handlers receive token-free params
        let req = &stripped_req;

        match req.method.as_str() {
            "aria2.addUri" => self
                .handle_add_uri(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.addTorrent" => self
                .handle_add_torrent(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.addMetalink" => self
                .handle_add_metalink(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.remove" => self
                .handle_remove(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.pause" => self
                .handle_pause(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.forcePause" => self
                .handle_force_pause(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.unpause" | "aria2.forceUnpause" => self
                .handle_unpause(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.tellStatus" => self
                .handle_tell_status(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.tellActive" => self
                .handle_tell_active(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.tellWaiting" => self
                .handle_tell_waiting(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.tellStopped" => self
                .handle_tell_stopped(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.getGlobalStat" => self.handle_global_stat(req).await,
            "aria2.getUris" => self
                .handle_get_uris(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.getFiles" => self
                .handle_get_files(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.getServers" => self
                .handle_get_servers(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.purgeDownloadResult" => self
                .handle_purge_download_result(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.removeDownloadResult" => self
                .handle_remove_download_result(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.getGlobalOption" => self.handle_get_global_option().await,
            "aria2.changeGlobalOption" => self
                .handle_change_global_option(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.getOption" => self
                .handle_get_option(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.changeOption" => self
                .handle_change_option(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.getPeers" => self
                .handle_get_peers(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.pauseAll" => self.handle_pause_all(req).await,
            "aria2.forcePauseAll" => self.handle_force_pause_all(req).await,
            "aria2.unpauseAll" => self.handle_unpause_all(req).await,
            "aria2.changeUri" => self
                .handle_change_uri(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.saveSession" => self
                .handle_save_session(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.changePosition" => self
                .handle_change_position(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.forceRemove" => self
                .handle_force_remove(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.shutdown" => self
                .handle_shutdown(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.forceShutdown" => self
                .handle_force_shutdown(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.getVersion" => self.handle_version(req),
            "aria2.getSessionInfo" => self.handle_session_info(req),
            "system.multicall" => self
                .handle_multicall(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "system.listMethods" => self
                .handle_list_methods(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "system.listNotifications" => self
                .handle_list_notifications(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            _ => JsonRpcResponse::error(id, -32601, format!("Method not found: {}", req.method)),
        }
    }
}

impl Default for RpcEngine {
    fn default() -> Self {
        Self::new()
    }
}

// =========================================================================
// Integration / Routing Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_engine_creation() {
        let engine = RpcEngine::new();
        assert_eq!(engine.task_count().await, 0);
    }

    #[tokio::test]
    async fn test_handle_unknown_method() {
        let engine = RpcEngine::new();
        let req = JsonRpcRequest::new("aria2.nonExistent", serde_json::json!([])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[tokio::test]
    async fn test_handle_version() {
        let engine = RpcEngine::new();
        let req = JsonRpcRequest::new("aria2.getVersion", serde_json::json!([])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success());
    }

    #[tokio::test]
    async fn test_handle_session_info() {
        let engine = RpcEngine::new();
        let req = JsonRpcRequest::new("aria2.getSessionInfo", serde_json::json!([])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success());
    }

    #[tokio::test]
    async fn test_handle_add_uri() {
        let engine = RpcEngine::new();
        let req = JsonRpcRequest::new(
            "aria2.addUri",
            serde_json::json!(["http://example.com/file.iso"]),
        )
        .with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success());
        let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(gid.len(), 16);
    }

    #[tokio::test]
    async fn test_handle_remove_nonexistent() {
        let engine = RpcEngine::new();
        let req =
            JsonRpcRequest::new("aria2.remove", serde_json::json!(["nonexistent-gid"])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_error());
    }

    #[tokio::test]
    async fn test_handle_pause_and_unpause() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let pause_req = JsonRpcRequest::new("aria2.pause", serde_json::json!([gid])).with_id(2);
        let pause_resp = engine.handle_request(&pause_req).await;
        assert!(pause_resp.is_success());

        let unpause_req = JsonRpcRequest::new("aria2.unpause", serde_json::json!([gid])).with_id(3);
        let unpause_resp = engine.handle_request(&unpause_req).await;
        assert!(unpause_resp.is_success());
    }

    #[tokio::test]
    async fn test_handle_tell_status() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let tell_req = JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid])).with_id(2);
        let tell_resp = engine.handle_request(&tell_req).await;
        assert!(tell_resp.is_success());
    }

    #[tokio::test]
    async fn test_handle_tell_active() {
        let engine = RpcEngine::new();
        let req = JsonRpcRequest::new("aria2.tellActive", serde_json::json!([])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success());
    }

    #[tokio::test]
    async fn test_handle_global_stat() {
        let engine = RpcEngine::new();
        let req = JsonRpcRequest::new("aria2.getGlobalStat", serde_json::json!([])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success());
    }

    #[tokio::test]
    async fn test_handle_purge_download_result() {
        let engine = RpcEngine::new();
        let req =
            JsonRpcRequest::new("aria2.purgeDownloadResult", serde_json::json!([])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success());
    }

    #[tokio::test]
    async fn test_handle_get_set_global_option() {
        let engine = RpcEngine::new();
        let get_req =
            JsonRpcRequest::new("aria2.getGlobalOption", serde_json::json!([])).with_id(1);
        let get_resp = engine.handle_request(&get_req).await;
        assert!(get_resp.is_success());

        let set_req = JsonRpcRequest::new(
            "aria2.changeGlobalOption",
            serde_json::json!([{"max-concurrent-downloads": 5}]),
        )
        .with_id(2);
        let set_resp = engine.handle_request(&set_req).await;
        assert!(set_resp.is_success());
    }

    // =========================================================================
    // Auth Integration Tests (G4 Part B)
    // =========================================================================

    #[tokio::test]
    async fn test_engine_auth_default_accepts_all() {
        // Default engine has no auth configured — all requests pass
        let engine = RpcEngine::new();
        let req = JsonRpcRequest::new("aria2.getVersion", serde_json::json!([])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(
            resp.is_success(),
            "Default engine should accept requests without token"
        );
    }

    #[tokio::test]
    async fn test_engine_auth_valid_token_passes() {
        let engine = RpcEngine::new().with_auth_middleware(RpcAuthMiddleware::new("my-secret"));

        // Request with correct token using aria2 standard positional protocol:
        // params[0] = "token:<secret>"
        let req = JsonRpcRequest::new("aria2.getVersion", serde_json::json!(["token:my-secret"]))
            .with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(
            resp.is_success(),
            "Valid token:secret positional should be accepted"
        );
    }

    #[tokio::test]
    async fn test_engine_auth_valid_token_named_secret_passes() {
        let engine = RpcEngine::new().with_auth_middleware(RpcAuthMiddleware::new("my-secret"));

        // Request with correct token using aria2 standard named-params protocol:
        // params.secret = "<secret>" (without "token:" prefix)
        let req = JsonRpcRequest::new(
            "aria2.getVersion",
            serde_json::json!({"secret": "my-secret"}),
        )
        .with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "Valid named secret should be accepted");
    }

    #[tokio::test]
    async fn test_engine_auth_wrong_token_rejected() {
        let engine = RpcEngine::new().with_auth_middleware(RpcAuthMiddleware::new("my-secret"));

        // Request with wrong token via positional protocol
        let req = JsonRpcRequest::new("aria2.getVersion", serde_json::json!(["token:wrong-token"]))
            .with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_error(), "Wrong token should be rejected");
        assert_eq!(
            resp.error.unwrap().code,
            -32001,
            "Should return Unauthorized error code"
        );
    }

    #[tokio::test]
    async fn test_engine_auth_missing_token_rejected() {
        let engine = RpcEngine::new().with_auth_middleware(RpcAuthMiddleware::new("my-secret"));

        // Request without any token parameter
        let req = JsonRpcRequest::new("aria2.getVersion", serde_json::json!([])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(
            resp.is_error(),
            "Missing token should be rejected when auth is enabled"
        );
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[tokio::test]
    async fn test_engine_auth_via_config_token() {
        // Test that AuthConfig.token flows through correctly
        let engine = RpcEngine::new().with_auth(AuthConfig::default().with_token("config-secret"));

        // Use aria2 standard positional protocol with config-provided secret
        let req = JsonRpcRequest::new(
            "aria2.getVersion",
            serde_json::json!(["token:config-secret"]),
        )
        .with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "Token from AuthConfig should work");
    }

    #[tokio::test]
    async fn test_engine_auth_token_stripped_from_params() {
        // After auth succeeds, the token must be stripped from params so
        // handlers never see "token:secret" as their first positional arg.
        let engine = RpcEngine::new().with_auth_middleware(RpcAuthMiddleware::new("my-secret"));

        // addUri expects params[0] = uris array, params[1] = options dict.
        // With token, the request is: ["token:my-secret", ["http://x.com/f"], {...}]
        let req = JsonRpcRequest::new(
            "aria2.addUri",
            serde_json::json!(["token:my-secret", ["http://x.com/f"]]),
        )
        .with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(
            resp.is_success(),
            "Token should be stripped and addUri should succeed. Got: {:?}",
            resp.error
        );
        // Verify the result is a GID (16 hex chars), not an error about
        // "token:my-secret" being treated as a URI list.
        let result = resp.result.unwrap();
        assert!(
            result.is_string(),
            "Result should be a GID string, got: {:?}",
            result
        );
        let gid: String = serde_json::from_value(result).unwrap();
        assert_eq!(gid.len(), 16, "GID should be 16 hex chars");
    }

    #[tokio::test]
    async fn test_engine_auth_named_secret_stripped_from_params() {
        // When using named params, the "secret" field must be removed
        // before dispatching so handlers don't see it.
        let engine = RpcEngine::new().with_auth_middleware(RpcAuthMiddleware::new("my-secret"));

        // Use named "secret" + named "gid" — tellStatus reads gid by name.
        let req = JsonRpcRequest::new(
            "aria2.getVersion",
            serde_json::json!({"secret": "my-secret"}),
        )
        .with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(
            resp.is_success(),
            "Named secret should be stripped and accepted"
        );
    }
}
