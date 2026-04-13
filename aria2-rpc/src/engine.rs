use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use super::server::{
    AuthConfig, CorsConfig, DownloadStatus, FileInfo, GlobalOptions, GlobalStat, PeerInfo,
    RpcAuthMiddleware, StatusInfo, TaskOptions, create_gid,
};
use super::websocket::{DownloadEvent, EventPublisher, EventType};
use aria2_core::engine::multi_file_layout::TorrentFileEntry;

// Include handler implementations from separate file (avoids module visibility issues)
include!("rpc_handlers.rs");

/// Core RPC engine that manages download tasks and handles aria2 protocol requests.
///
/// This is the main orchestrator that:
/// - Maintains task state (active downloads, stopped tasks)
/// - Routes incoming RPC requests to appropriate handlers
/// - Provides progress tracking and status queries
/// - Publishes events via WebSocket for real-time notifications
///
/// Handler implementations are in [rpc_handlers](crate::engine::rpc_handlers) module.
pub struct RpcEngine {
    /// Active download tasks keyed by GID
    pub(super) tasks: Arc<RwLock<HashMap<String, TaskState>>>,
    /// Global configuration options
    pub(super) global_opts: GlobalOptions,
    /// Per-task configuration options
    pub(super) task_opts: TaskOptions,
    /// Completed/stopped task results
    pub(super) stopped_tasks: Arc<RwLock<Vec<StatusInfo>>>,
    /// Event publisher for WebSocket notifications
    pub(super) event_publisher: Arc<EventPublisher>,
    /// Authentication middleware for token-based RPC auth
    pub(super) auth_middleware: RpcAuthMiddleware,
}

/// Internal state for an active download task.
///
/// Contains both static metadata (GID, URIs, options) and dynamic
/// progress fields (speeds, lengths, connections) that are updated
/// by the download engine during execution.
pub(super) struct TaskState {
    /// Current status information with metadata
    pub(super) status: StatusInfo,
    /// Configuration options specific to this task
    #[allow(dead_code)]
    pub(super) options: HashMap<String, serde_json::Value>,
    /// URI list for this download
    #[allow(dead_code)]
    pub(super) uris: Vec<String>,
    /// Torrent file entries (for BitTorrent downloads)
    pub(super) torrent_files: Option<Vec<TorrentFileEntry>>,
    // === Dynamic progress fields (updated by download engine) ===
    pub(super) total_length: u64,
    pub(super) completed_length: u64,
    pub(super) upload_length: u64,
    pub(super) download_speed: u64,
    pub(super) upload_speed: u64,
    pub(super) connections: u16,
    /// Peer list for BitTorrent downloads
    pub(super) peers: Vec<PeerInfo>,
}

impl TaskState {
    fn new(
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
        }
    }

    /// Update the StatusInfo snapshot from current internal state.
    ///
    /// Called before returning status to ensure all progress fields
    /// are reflected in the response.
    fn update_status_info(&mut self) {
        let mut status = StatusInfo::new(&self.status.gid)
            .with_status(self.status.status)
            .with_total_length(self.total_length)
            .with_completed_length(self.completed_length)
            .with_upload_length(self.upload_length)
            .with_download_speed(self.download_speed)
            .with_upload_speed(self.upload_speed)
            .with_connections(self.connections)
            .with_dir(self.status.dir.clone().unwrap_or_default())
            .with_files(self.status.files.clone().unwrap_or_default());
        if let Some(ref tf) = self.torrent_files {
            status = status.with_torrent_files(tf.clone());
        }
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
    /// Create a new RpcEngine instance with empty state.
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            global_opts: Arc::new(RwLock::new(HashMap::new())),
            task_opts: Arc::new(RwLock::new(HashMap::new())),
            stopped_tasks: Arc::new(RwLock::new(Vec::new())),
            event_publisher: Arc::new(EventPublisher::default()),
            auth_middleware: RpcAuthMiddleware::default(),
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
        let token = req.params.get("token").and_then(|v| v.as_str());
        if let Err(auth_err) = self.auth_middleware.validate(token) {
            return auth_err.into_response(req.id.clone());
        }

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
            "aria2.pause" | "aria2.forcePause" => self
                .handle_pause(req)
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
            "aria2.getGlobalStat" => self.handle_global_stat().await,
            "aria2.purgeDownloadResult" => self.handle_purge_download_result(),
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
            "aria2.pauseAll" => self.handle_pause_all().await,
            "aria2.unpauseAll" => self.handle_unpause_all().await,
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
            "aria2.getVersion" => self.handle_version(),
            "aria2.getSessionInfo" => self.handle_session_info(),
            "system.multicall" => self
                .handle_multicall(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            _ => JsonRpcResponse::error(id, -32601, format!("Method not found: {}", req.method)),
        }
    }

    /// Internal helper to add a new download task.
    ///
    /// Creates a new GID, initializes task state, stores it in the task map,
    /// and publishes a DownloadStart event.
    async fn add_task(
        &self,
        uris: Vec<String>,
        options: HashMap<String, serde_json::Value>,
    ) -> Result<String, JsonRpcError> {
        let gid = create_gid();
        let dir = options
            .get("dir")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();
        let status = StatusInfo::new(&gid)
            .with_status(DownloadStatus::Active)
            .with_dir(dir)
            .with_total_length(0)
            .with_completed_length(0)
            .with_files(vec![FileInfo::new("", 0)]);
        let state = TaskState::new(status, options, uris);
        {
            let mut tasks = self.tasks.write().await;
            tasks.insert(gid.clone(), state);
        }
        let _ = self.event_publisher.publish(
            EventType::DownloadStart,
            DownloadEvent::download_start(&gid, vec![]),
        );
        Ok(gid)
    }

    /// Internal helper to get current status info for a task.
    ///
    /// Updates the status snapshot from internal state before returning.
    async fn get_status(&self, gid: &str) -> Option<StatusInfo> {
        let mut tasks = self.tasks.write().await;
        let state = tasks.get_mut(gid)?;
        state.update_status_info();
        Some(state.status.clone())
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
