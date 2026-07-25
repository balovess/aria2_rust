use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use super::json_rpc::{JsonRpcRequest, JsonRpcResponse};
use super::rpc_helpers::to_aria2_wire_format;
use super::server::{AuthConfig, CorsConfig, RpcAuthMiddleware};
use super::types::{GlobalOptions, PeerInfo, StatusInfo, TaskOptions};
use super::websocket::EventPublisher;
#[cfg(feature = "bittorrent")]
use aria2_core::TorrentFileEntry;
use aria2_core::config::OptionRegistry;
use aria2_core::engine::command::Command;
use aria2_core::request::request_group_man::RequestGroupMan;

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
    pub event_publisher: Arc<EventPublisher>,
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
    /// Cumulative count of stopped downloads since session start (atomic).
    pub(crate) num_stopped_total: AtomicUsize,
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
    #[cfg(feature = "bittorrent")]
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
            #[cfg(feature = "bittorrent")]
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
    /// Called before returning status to ensure all progress fields
    /// are reflected in the response.
    pub(crate) fn update_status_info(&mut self) {
        let status = StatusInfo::new(&self.status.gid)
            .with_status(self.status.status.clone())
            .with_total_length(self.total_length)
            .with_completed_length(self.completed_length)
            .with_upload_length(self.upload_length)
            .with_download_speed(self.download_speed)
            .with_upload_speed(self.upload_speed)
            .with_connections(self.connections)
            .with_dir(self.status.dir.clone().unwrap_or_default())
            .with_files(self.status.files.clone().unwrap_or_default());
        // TODO: Construct BittorrentInfo from self.torrent_files (Task 19)
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
            num_stopped_total: AtomicUsize::new(0),
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
    #[cfg(feature = "bittorrent")]
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
    pub async fn handle_request(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let id = req.id.clone().unwrap_or(serde_json::Value::Null);

        // Authenticate: extract token from params and validate
        // Support both array-style ("token:xxx" as first param element)
        // and object-style ({"token": "xxx"}) params for backward compatibility.
        let token = if let Some(arr) = req.params.as_array() {
            arr.first()
                .and_then(|v| v.as_str())
                .and_then(|s| s.strip_prefix("token:"))
        } else {
            req.params.get("token").and_then(|v| v.as_str())
        };
        if let Err(auth_err) = self.auth_middleware.validate(token) {
            return auth_err.into_response(req.id.clone());
        }

        // Strip the "token:xxx" prefix from params[0] so method handlers
        // never see it — matching C++ aria2's authorize() behaviour which
        // removes the token from the params array before dispatching.
        let stripped_req;
        let dispatch_req = if let Some(arr) = req.params.as_array() {
            if arr
                .first()
                .and_then(|v| v.as_str())
                .map_or(false, |s| s.starts_with("token:"))
            {
                let mut new_params = arr.clone();
                new_params.remove(0);
                stripped_req = JsonRpcRequest {
                    version: req.version.clone(),
                    method: req.method.clone(),
                    params: serde_json::Value::Array(new_params),
                    id: req.id.clone(),
                };
                &stripped_req
            } else {
                req
            }
        } else {
            req
        };

        let mut resp = match dispatch_req.method.as_str() {
            "aria2.addUri" => self
                .handle_add_uri(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.addTorrent" => self
                .handle_add_torrent(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.addMetalink" => self
                .handle_add_metalink(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.remove" => self
                .handle_remove(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.pause" => self
                .handle_pause(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.forcePause" => self
                .handle_force_pause(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.unpause" | "aria2.forceUnpause" => self
                .handle_unpause(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.tellStatus" => self
                .handle_tell_status(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.tellActive" => self
                .handle_tell_active(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.tellWaiting" => self
                .handle_tell_waiting(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.tellStopped" => self
                .handle_tell_stopped(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.getGlobalStat" => self.handle_global_stat(dispatch_req).await,
            "aria2.getUris" => self
                .handle_get_uris(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.getFiles" => self
                .handle_get_files(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.getServers" => self
                .handle_get_servers(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.purgeDownloadResult" => self
                .handle_purge_download_result(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.removeDownloadResult" => self
                .handle_remove_download_result(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.getGlobalOption" => self.handle_get_global_option(dispatch_req).await,
            "aria2.changeGlobalOption" => self
                .handle_change_global_option(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.getOption" => self
                .handle_get_option(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.changeOption" => self
                .handle_change_option(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.getPeers" => self
                .handle_get_peers(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.pauseAll" => self.handle_pause_all(dispatch_req).await,
            "aria2.forcePauseAll" => self.handle_force_pause_all(dispatch_req).await,
            "aria2.unpauseAll" => self.handle_unpause_all(dispatch_req).await,
            "aria2.changeUri" => self
                .handle_change_uri(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.saveSession" => self
                .handle_save_session(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.changePosition" => self
                .handle_change_position(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.forceRemove" => self
                .handle_force_remove(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.shutdown" => self
                .handle_shutdown(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.forceShutdown" => self
                .handle_force_shutdown(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "aria2.getVersion" => self.handle_version(dispatch_req),
            "aria2.getSessionInfo" => self.handle_session_info(dispatch_req),
            "system.multicall" => self
                .handle_multicall(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "system.listMethods" => self
                .handle_list_methods(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "system.listNotifications" => self
                .handle_list_notifications(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            _ => JsonRpcResponse::error(
                id,
                -32601,
                format!("Method not found: {}", dispatch_req.method),
            ),
        };
        // Apply aria2 wire format: convert all numbers to strings and booleans to
        // "true"/"false" strings, matching the original aria2 JSON-RPC response format.
        if let Some(result) = resp.result.take() {
            resp.result = Some(to_aria2_wire_format(result));
        }
        resp
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

    /// Verify that `aria2.remove` cancels the task's `CancellationToken`.
    ///
    /// Before the fix, `handle_remove` removed the task from the map but
    /// never called `cancel_token.cancel()`, so the running `DownloadCommand`
    /// had no way to know it should stop. The download kept running in the
    /// background until it finished.
    #[tokio::test]
    async fn test_handle_remove_cancels_token() {
        let engine = RpcEngine::new();

        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        // Clone the CancellationToken before remove so we can inspect it
        // afterwards (the task is removed from the map by handle_remove).
        let cancel_token = {
            let tasks = engine.tasks.read().await;
            tasks
                .get(&gid)
                .and_then(|s| s.cancel_token.clone())
                .expect("TaskState should have a cancel_token")
        };
        assert!(
            !cancel_token.is_cancelled(),
            "token should not be cancelled before remove"
        );

        let remove_req = JsonRpcRequest::new("aria2.remove", serde_json::json!([gid])).with_id(2);
        let remove_resp = engine.handle_request(&remove_req).await;
        assert!(remove_resp.is_success(), "aria2.remove should succeed");

        assert!(
            cancel_token.is_cancelled(),
            "CancellationToken must be cancelled after aria2.remove so the running DownloadCommand can stop"
        );
        assert_eq!(
            engine.task_count().await,
            0,
            "task should be removed from the map"
        );
    }

    /// Verify that `aria2.forceRemove` cancels the task's `CancellationToken`.
    ///
    /// Before the fix, `handle_force_remove` set the status to `Removed` but
    /// never called `cancel_token.cancel()`.
    #[tokio::test]
    async fn test_handle_force_remove_cancels_token() {
        let engine = RpcEngine::new();

        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let cancel_token = {
            let tasks = engine.tasks.read().await;
            tasks
                .get(&gid)
                .and_then(|s| s.cancel_token.clone())
                .expect("TaskState should have a cancel_token")
        };
        assert!(
            !cancel_token.is_cancelled(),
            "token should not be cancelled before forceRemove"
        );

        let remove_req =
            JsonRpcRequest::new("aria2.forceRemove", serde_json::json!([gid])).with_id(2);
        let remove_resp = engine.handle_request(&remove_req).await;
        assert!(remove_resp.is_success(), "aria2.forceRemove should succeed");

        assert!(
            cancel_token.is_cancelled(),
            "CancellationToken must be cancelled after aria2.forceRemove so the running DownloadCommand can stop"
        );
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

        // Request with correct token in params
        let req = JsonRpcRequest::new(
            "aria2.getVersion",
            serde_json::json!({"token": "my-secret"}),
        )
        .with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "Valid token should be accepted");
    }

    #[tokio::test]
    async fn test_engine_auth_wrong_token_rejected() {
        let engine = RpcEngine::new().with_auth_middleware(RpcAuthMiddleware::new("my-secret"));

        // Request with wrong token
        let req = JsonRpcRequest::new(
            "aria2.getVersion",
            serde_json::json!({"token": "wrong-token"}),
        )
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

        // Use object-style params where token is a named field
        let req = JsonRpcRequest::new(
            "aria2.getVersion",
            serde_json::json!({"token": "config-secret"}),
        )
        .with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "Token from AuthConfig should work");
    }
}
