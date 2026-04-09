use base64::Engine;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use super::server::{
    create_gid, AuthConfig, CorsConfig, DownloadStatus, FileInfo, GlobalOptions, GlobalStat,
    PeerInfo, StatusInfo, TaskOptions,
};
use super::websocket::{DownloadEvent, EventPublisher, EventType};
use aria2_core::engine::multi_file_layout::TorrentFileEntry;

pub struct RpcEngine {
    tasks: Arc<RwLock<HashMap<String, TaskState>>>,
    global_opts: GlobalOptions,
    task_opts: TaskOptions,
    stopped_tasks: Arc<RwLock<Vec<StatusInfo>>>,
    event_publisher: Arc<EventPublisher>,
}

struct TaskState {
    status: StatusInfo,
    #[allow(dead_code)]
    options: HashMap<String, serde_json::Value>,
    #[allow(dead_code)]
    uris: Vec<String>,
    torrent_files: Option<Vec<TorrentFileEntry>>,
    total_length: u64,
    completed_length: u64,
    upload_length: u64,
    download_speed: u64,
    upload_speed: u64,
    connections: u16,
    peers: Vec<PeerInfo>,
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

    /// Update progress fields (typically called by download engine)
    pub fn update_progress(&mut self, total: u64, completed: u64, uploaded: u64,
                           dl_speed: u64, ul_speed: u64, connections: u16) {
        self.total_length = total;
        self.completed_length = completed;
        self.upload_length = uploaded;
        self.download_speed = dl_speed;
        self.upload_speed = ul_speed;
        self.connections = connections;
    }
}

impl RpcEngine {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            global_opts: Arc::new(RwLock::new(HashMap::new())),
            task_opts: Arc::new(RwLock::new(HashMap::new())),
            stopped_tasks: Arc::new(RwLock::new(Vec::new())),
            event_publisher: Arc::new(EventPublisher::default()),
        }
    }

    pub fn with_auth(self, auth: AuthConfig) -> Self {
        let _ = auth;
        self
    }
    pub fn with_cors(self, cors: CorsConfig) -> Self {
        let _ = cors;
        self
    }

    pub fn publisher(&self) -> &EventPublisher {
        &self.event_publisher
    }
    pub async fn task_count(&self) -> usize {
        self.tasks.read().await.len()
    }

    /// Update progress for a specific task (called by download engine)
    pub async fn update_task_progress(&self, gid: &str, total: u64, completed: u64,
                                      uploaded: u64, dl_speed: u64, ul_speed: u64,
                                      connections: u16) -> bool {
        let mut tasks = self.tasks.write().await;
        if let Some(state) = tasks.get_mut(gid) {
            state.update_progress(total, completed, uploaded, dl_speed, ul_speed, connections);
            true
        } else {
            false
        }
    }

    pub async fn set_task_torrent_files(&self, gid: &str, files: Vec<TorrentFileEntry>) -> bool {
        let mut tasks = self.tasks.write().await;
        if let Some(state) = tasks.get_mut(gid) {
            state.torrent_files = Some(files);
            true
        } else {
            false
        }
    }

    pub async fn set_task_peers(&self, gid: &str, peers: Vec<PeerInfo>) -> bool {
        let mut tasks = self.tasks.write().await;
        if let Some(state) = tasks.get_mut(gid) {
            state.peers = peers;
            true
        } else {
            false
        }
    }

    pub async fn handle_request(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let id = req.id.clone().unwrap_or(serde_json::Value::Null);
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
            "aria2.pauseAll" => self.handle_pause_all().await.into(),
            "aria2.unpauseAll" => self.handle_unpause_all().await.into(),
            "aria2.changeUri" => self
                .handle_change_uri(req)
                .await
                .unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.getVersion" => self.handle_version(),
            "aria2.getSessionInfo" => self.handle_session_info(),
            _ => JsonRpcResponse::error(id, -32601, format!("Method not found: {}", req.method)),
        }
    }

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

    async fn handle_add_uri(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let uri: String = req.get_param(0)?;
        let opts: HashMap<String, serde_json::Value> = req.get_param_or_default(1);
        let gid = self.add_task(vec![uri], opts).await?;
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            gid,
        ))
    }

    async fn handle_add_torrent(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let torrent_data: String = req.get_param(0)?;
        let opts: HashMap<String, serde_json::Value> = req.get_param_or_default(1);
        let _dir = opts
            .get("dir")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();

        let decoded_bytes = if torrent_data.starts_with("data:") {
            base64::engine::general_purpose::STANDARD
                .decode(torrent_data.split(',').nth(1).unwrap_or(""))
                .map_err(|e| JsonRpcError::InvalidParams(format!("base64 decode failed: {}", e)))?
        } else {
            base64::engine::general_purpose::STANDARD
                .decode(&torrent_data)
                .map_err(|e| JsonRpcError::InvalidParams(format!("base64 decode failed: {}", e)))?
        };

        if decoded_bytes.len() < 3
            || decoded_bytes[0] != b'd'
            || decoded_bytes[1] != b'8'
            || decoded_bytes[2] != b':'
        {
            return Err(JsonRpcError::InvalidParams(
                "Invalid BEncode data (not a .torrent file)".into(),
            ));
        }

        let gid = self
            .add_task(
                vec![format!(
                    "torrent://{}",
                    &decoded_bytes[..std::cmp::min(32, decoded_bytes.len())]
                        .iter()
                        .map(|b| format!("{:02x}", b))
                        .collect::<String>()
                )],
                opts,
            )
            .await?;
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            gid,
        ))
    }

    async fn handle_add_metalink(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let metalink_data: String = req.get_param(0)?;
        let opts: HashMap<String, serde_json::Value> = req.get_param_or_default(1);

        let decoded_bytes = if metalink_data.starts_with("data:") {
            base64::engine::general_purpose::STANDARD
                .decode(metalink_data.split(',').nth(1).unwrap_or(""))
                .map_err(|e| JsonRpcError::InvalidParams(format!("base64 decode failed: {}", e)))?
        } else {
            base64::engine::general_purpose::STANDARD
                .decode(&metalink_data)
                .map_err(|e| JsonRpcError::InvalidParams(format!("base64 decode failed: {}", e)))?
        };

        let preview = String::from_utf8_lossy(&decoded_bytes[..decoded_bytes.len().min(200)]);
        if !preview.to_lowercase().contains("<metalink")
            && !preview.contains("urn:ietf:params:xml:ns:metalink")
        {
            return Err(JsonRpcError::InvalidParams(
                "Invalid Metalink XML data".into(),
            ));
        }

        let gid = self
            .add_task(vec!["metalink://download".to_string()], opts)
            .await?;
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            gid,
        ))
    }

    async fn handle_remove(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let mut tasks = self.tasks.write().await;
        match tasks.remove(&gid) {
            Some(_) => Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::json!([gid]),
            )),
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    async fn handle_pause(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let mut tasks = self.tasks.write().await;
        if let Some(state) = tasks.get_mut(&gid) {
            state.status.status = DownloadStatus::Paused;
            Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::json!([gid]),
            ))
        } else {
            Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            )))
        }
    }

    async fn handle_unpause(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let mut tasks = self.tasks.write().await;
        if let Some(state) = tasks.get_mut(&gid) {
            state.status.status = DownloadStatus::Active;
            Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::json!([gid]),
            ))
        } else {
            Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            )))
        }
    }

    async fn get_status(&self, gid: &str) -> Option<StatusInfo> {
        let mut tasks = self.tasks.write().await;
        let state = tasks.get_mut(gid)?;
        state.update_status_info();
        Some(state.status.clone())
    }

    async fn handle_tell_status(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        match self.get_status(&gid).await {
            Some(status) => Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::to_value(status).unwrap(),
            )),
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    async fn handle_tell_active(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let tasks = self.tasks.read().await;
        let active: Vec<StatusInfo> = tasks
            .values()
            .filter(|s| s.status.status.is_active())
            .map(|s| s.status.clone())
            .collect();
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::to_value(active).unwrap(),
        ))
    }

    async fn handle_tell_waiting(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let offset: usize = req.get_param_or_default(0);
        let num: usize = req.get_param_or_default(1);
        let tasks = self.tasks.read().await;
        let waiting: Vec<StatusInfo> = tasks
            .values()
            .filter(|s| s.status.status == DownloadStatus::Waiting)
            .skip(offset.min(tasks.len()))
            .take(num)
            .map(|s| s.status.clone())
            .collect();
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::to_value(waiting).unwrap(),
        ))
    }

    async fn handle_tell_stopped(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let offset: usize = req.get_param_or_default(0);
        let num: usize = req.get_param_or_default(1);
        let stopped = self.stopped_tasks.read().await;
        let result: Vec<&StatusInfo> = stopped
            .iter()
            .skip(offset.min(stopped.len()))
            .take(num)
            .collect();
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::to_value(result).unwrap(),
        ))
    }

    async fn handle_global_stat(&self) -> JsonRpcResponse {
        let tasks = self.tasks.read().await;
        let (active, waiting): (Vec<_>, Vec<_>) =
            tasks.values().partition(|s| s.status.status.is_active());
        let stat = GlobalStat {
            download_speed: 1024 * 1024,
            upload_speed: 512 * 1024,
            num_active: active.len(),
            num_waiting: waiting.len(),
            num_stopped: 10,
            num_stopped_total: 42,
        };
        JsonRpcResponse::success(serde_json::Value::Null, stat.to_json_value())
    }

    fn handle_purge_download_result(&self) -> JsonRpcResponse {
        JsonRpcResponse::success(serde_json::Value::Null, "OK")
    }

    async fn handle_remove_download_result(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let _gid: String = req.get_param(0)?;
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            "OK",
        ))
    }

    async fn handle_get_global_option(&self) -> JsonRpcResponse {
        let opts = self.global_opts.read().await;
        JsonRpcResponse::success(
            serde_json::Value::Null,
            serde_json::to_value(&*opts).unwrap_or(serde_json::json!({})),
        )
    }

    async fn handle_change_global_option(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let new_opts: HashMap<String, serde_json::Value> = req.get_param(0)?;
        let mut opts = self.global_opts.write().await;
        for (k, v) in new_opts {
            opts.insert(k, v);
        }
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            "OK",
        ))
    }

    async fn handle_get_option(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let task_opts = self.task_opts.read().await;
        match task_opts.get(&gid) {
            Some(opts) => Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::to_value(opts).unwrap(),
            )),
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    async fn handle_change_option(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let changes: HashMap<String, serde_json::Value> = req.get_param(1)?;

        const VALID_OPTION_KEYS: &[&str] = &[
            "split", "max-connection-per-server", "max-download-limit",
            "max-upload-limit", "dir", "out", "seed-time", "seed-ratio",
            "bt-force-encrypt", "bt-require-crypto", "enable-dht",
            "dht-listen-port", "enable-public-trackers",
            "bt-piece-selection-strategy", "bt-endgame-threshold",
            "max-retries", "retry-wait", "http-proxy", "dht-file-path",
            "bt-max-upload-slots", "bt-optimistic-unchoke-interval", "bt-snubbed-timeout",
        ];

        for key in changes.keys() {
            if !VALID_OPTION_KEYS.contains(&key.as_str()) {
                return Err(JsonRpcError::InvalidParams(format!("Unknown option: {}", key)));
            }
        }

        let mut task_opts = self.task_opts.write().await;
        let entry = task_opts.entry(gid).or_insert_with(HashMap::new);
        for (k, v) in changes {
            entry.insert(k, v);
        }
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            "OK",
        ))
    }

    async fn handle_get_peers(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let tasks = self.tasks.read().await;
        match tasks.get(&gid) {
            Some(state) => Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::to_value(&state.peers).unwrap(),
            )),
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    async fn handle_pause_all(&self) -> JsonRpcResponse {
        let mut tasks = self.tasks.write().await;
        let mut count = 0usize;
        for state in tasks.values_mut() {
            if state.status.status == DownloadStatus::Active {
                state.status.status = DownloadStatus::Paused;
                let _ = self.event_publisher.publish(
                    EventType::DownloadPause,
                    DownloadEvent::download_pause(&state.status.gid),
                );
                count += 1;
            }
        }
        JsonRpcResponse::success(serde_json::Value::Null, format!("OK. {} tasks paused.", count))
    }

    async fn handle_unpause_all(&self) -> JsonRpcResponse {
        let mut tasks = self.tasks.write().await;
        let mut count = 0usize;
        for state in tasks.values_mut() {
            if state.status.status == DownloadStatus::Paused {
                state.status.status = DownloadStatus::Active;
                count += 1;
            }
        }
        JsonRpcResponse::success(serde_json::Value::Null, format!("OK. {} tasks resumed.", count))
    }

    async fn handle_change_uri(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let _file_index: usize = req.get_param_or_default(1);
        let del_uris: Option<Vec<String>> = req
            .get_param::<serde_json::Value>(2)
            .ok()
            .and_then(|v| serde_json::from_value(v).ok());
        let add_uris: Option<Vec<String>> = req
            .get_param::<serde_json::Value>(3)
            .ok()
            .and_then(|v| serde_json::from_value(v).ok());

        let mut tasks = self.tasks.write().await;
        let state = tasks.get_mut(&gid).ok_or_else(|| {
            JsonRpcError::MethodNotFound(format!("GID {} not found", gid))
        })?;

        if let Some(to_remove) = del_uris {
            state.uris.retain(|u| !to_remove.contains(u));
        }

        if let Some(to_add) = add_uris {
            state.uris.extend(to_add);
        }

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::json!([gid, 0]),
        ))
    }

    fn handle_version(&self) -> JsonRpcResponse {
        JsonRpcResponse::success(
            serde_json::Value::Null,
            serde_json::json!({
                "version": "1.37.0-Rust",
                "enabledFeatures": ["http", "https", "ftp", "bittorrent", "metalink", "sftp"],
                "session": "aria2-rpc"
            }),
        )
    }

    fn handle_session_info(&self) -> JsonRpcResponse {
        use std::time::{SystemTime, UNIX_EPOCH};
        let session_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| format!("session-{:x}", d.as_nanos()))
            .unwrap_or_else(|_| "session-unknown".to_string());
        JsonRpcResponse::success(
            serde_json::Value::Null,
            serde_json::json!({"sessionId": session_id}),
        )
    }
}

impl Default for RpcEngine {
    fn default() -> Self {
        Self::new()
    }
}

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

    #[tokio::test]
    async fn test_handle_get_set_option() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let set_req = JsonRpcRequest::new(
            "aria2.changeOption",
            serde_json::json!([gid, {"max-download-limit": 1048576}]),
        )
        .with_id(2);
        let set_resp = engine.handle_request(&set_req).await;
        assert!(set_resp.is_success());

        let get_req = JsonRpcRequest::new("aria2.getOption", serde_json::json!([gid])).with_id(3);
        let get_resp = engine.handle_request(&get_req).await;
        assert!(get_resp.is_success());
    }

    #[tokio::test]
    async fn test_multiple_tasks() {
        let engine = RpcEngine::new();
        for i in 0..5 {
            let req = JsonRpcRequest::new(
                "aria2.addUri",
                serde_json::json!([format!("http://x.com/{}", i)]),
            )
            .with_id(i);
            engine.handle_request(&req).await;
        }
        assert_eq!(engine.task_count().await, 5);
    }

    #[tokio::test]
    async fn test_engine_default() {
        let engine = RpcEngine::default();
        assert_eq!(engine.task_count().await, 0);
    }

    #[tokio::test]
    async fn test_handle_add_torrent() {
        let engine = RpcEngine::new();
        let fake_torrent_bencode = "d8:announce40:http://tracker.example.com/announce4:info6:lengthi1000e12:piece lengthi32768e6:pieces20:00000000000000000000000ee";
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(fake_torrent_bencode.as_bytes());
        let req = JsonRpcRequest::new("aria2.addTorrent", serde_json::json!([encoded])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(
            resp.is_success(),
            "addTorrent should succeed for valid BEncode data"
        );
        let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(!gid.is_empty());
        assert_eq!(engine.task_count().await, 1);
    }

    #[tokio::test]
    async fn test_handle_add_torrent_invalid_data() {
        let engine = RpcEngine::new();
        let not_torrent =
            base64::engine::general_purpose::STANDARD.encode("this is not a torrent file");
        let req =
            JsonRpcRequest::new("aria2.addTorrent", serde_json::json!([not_torrent])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(
            resp.is_error(),
            "addTorrent should fail for non-BEncode data"
        );
    }

    #[tokio::test]
    async fn test_handle_add_metalink() {
        let engine = RpcEngine::new();
        let metalink_xml = r#"<?xml version="1.0"?><metalink xmlns="urn:ietf:params:xml:ns:metalink"><files><file name="test.bin"><size>1024</size><url priority="1">http://example.com/test.bin</url></file></files></metalink>"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(metalink_xml.as_bytes());
        let req = JsonRpcRequest::new("aria2.addMetalink", serde_json::json!([encoded])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(
            resp.is_success(),
            "addMetalink should succeed for valid Metalink XML"
        );
        let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(!gid.is_empty());
        assert_eq!(engine.task_count().await, 1);
    }

    #[tokio::test]
    async fn test_handle_add_metalink_invalid_data() {
        let engine = RpcEngine::new();
        let not_metalink =
            base64::engine::general_purpose::STANDARD.encode("this is not metalink xml");
        let req =
            JsonRpcRequest::new("aria2.addMetalink", serde_json::json!([not_metalink])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_error(), "addMetalink should fail for non-XML data");
    }

    #[tokio::test]
    async fn test_tell_status_has_real_progress_data() {
        let engine = RpcEngine::new();

        // Add a task
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/large.iso"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        // Update progress with known values
        engine.update_task_progress(
            &gid,
            10485760,  // total_length: 10MB
            5242880,   // completed_length: 5MB
            1024,      // upload_length: 1KB
            1048576,   // download_speed: 1MB/s
            512,       // upload_speed: 512B/s
            3,         // connections: 3 peers
        ).await;

        // Query status and verify real progress data is returned
        let tell_req = JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid])).with_id(2);
        let tell_resp = engine.handle_request(&tell_req).await;
        assert!(tell_resp.is_success(), "tellStatus should succeed");

        let status_val = tell_resp.result.unwrap();
        let status: StatusInfo = serde_json::from_value(status_val).unwrap();

        // Verify all progress fields contain the expected values
        assert_eq!(status.total_length, Some(10485760), "total_length should be 10MB");
        assert_eq!(status.completed_length, Some(5242880), "completed_length should be 5MB (50%)");
        assert_eq!(status.upload_length, Some(1024), "upload_length should be 1KB");
        assert_eq!(status.download_speed, Some(1048576), "download_speed should be 1MB/s");
        assert_eq!(status.upload_speed, Some(512), "upload_speed should be 512B/s");
        assert_eq!(status.connections, Some(3), "connections should be 3");

        // Verify progress calculation works correctly
        let expected_percent = (5242880.0 / 10485760.0) * 100.0;
        assert!((status.progress_percent() - expected_percent).abs() < 0.01,
                "progress percent should be ~50%");
    }

    #[tokio::test]
    async fn test_tell_status_zero_for_nonexistent_gid() {
        let engine = RpcEngine::new();

        // Query non-existent GID should return error
        let tell_req = JsonRpcRequest::new(
            "aria2.tellStatus",
            serde_json::json!(["nonexistent-gid-12345"])
        ).with_id(1);
        let tell_resp = engine.handle_request(&tell_req).await;

        assert!(tell_resp.is_error(), "tellStatus should fail for non-existent GID");
        assert_eq!(tell_resp.error.unwrap().code, -32601,
                   "error code should be MethodNotFound (-32601)");
    }

    #[tokio::test]
    async fn test_tell_status_includes_upload_fields() {
        let engine = RpcEngine::new();

        // Add a task
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://torrent.example.com/file.torrent"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        // Simulate BT upload scenario
        engine.update_task_progress(
            &gid,
            1073741824,  // total: 1GB
            1073741824,  // completed: 100% (seeding)
            536870912,   // uploaded: 512MB (seeding contribution)
            0,           // download_speed: 0 (seeding)
            1048576,     // upload_speed: 1MB/s (seeding speed)
            10,          // connections: 10 peers
        ).await;

        // Verify upload fields are present and correct
        let tell_req = JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid])).with_id(2);
        let tell_resp = engine.handle_request(&tell_req).await;
        assert!(tell_resp.is_success());

        let status_val = tell_resp.result.unwrap();
        let status: StatusInfo = serde_json::from_value(status_val).unwrap();

        // Upload fields must be present in response
        assert!(status.upload_length.is_some(), "upload_length field must be present");
        assert!(status.upload_speed.is_some(), "upload_speed field must be present");
        assert_eq!(status.upload_length, Some(536870912),
                   "upload_length should reflect seeding contribution");
        assert_eq!(status.upload_speed, Some(1048576),
                   "upload_speed should show current seeding rate");
        assert_eq!(status.connections, Some(10),
                   "connections should show peer count");
    }

    #[tokio::test]
    async fn test_get_peers_returns_peer_list() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f.torrent"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let peers = vec![
            PeerInfo {
                peer_id: "p1".to_string(),
                ip: "10.0.0.1".to_string(),
                port: 6881,
                am_choking: false,
                peer_choking: true,
                download_speed: 100000,
                upload_speed: 50000,
            },
            PeerInfo {
                peer_id: "p2".to_string(),
                ip: "10.0.0.2".to_string(),
                port: 6882,
                am_choking: true,
                peer_choking: false,
                download_speed: 200000,
                upload_speed: 75000,
            },
        ];
        engine.set_task_peers(&gid, peers.clone()).await;

        let req = JsonRpcRequest::new("aria2.getPeers", serde_json::json!([gid])).with_id(2);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "getPeers should succeed for existing GID");

        let result_peers: Vec<PeerInfo> = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(result_peers.len(), 2, "Should return 2 peers");
        assert_eq!(result_peers[0].peer_id, "p1");
        assert_eq!(result_peers[1].ip, "10.0.0.2");
    }

    #[tokio::test]
    async fn test_get_peers_unknown_gid() {
        let engine = RpcEngine::new();
        let req =
            JsonRpcRequest::new("aria2.getPeers", serde_json::json!(["nonexistent-gid"])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_error(), "getPeers should fail for non-existent GID");
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[tokio::test]
    async fn test_pause_all_pauses_active_tasks() {
        let engine = RpcEngine::new();
        for i in 0..3 {
            let req = JsonRpcRequest::new(
                "aria2.addUri",
                serde_json::json!([format!("http://x.com/{}", i)]),
            )
            .with_id(i);
            engine.handle_request(&req).await;
        }
        assert_eq!(engine.task_count().await, 3);

        let pause_req = JsonRpcRequest::new("aria2.pauseAll", serde_json::json!([])).with_id(10);
        let pause_resp = engine.handle_request(&pause_req).await;
        assert!(pause_resp.is_success());

        let tell_req = JsonRpcRequest::new("aria2.tellActive", serde_json::json!([])).with_id(11);
        let tell_resp = engine.handle_request(&tell_req).await;
        assert!(tell_resp.is_success());
        let active: Vec<StatusInfo> = serde_json::from_value(tell_resp.result.unwrap()).unwrap();
        assert_eq!(active.len(), 0, "No tasks should be active after pauseAll");
    }

    #[tokio::test]
    async fn test_unpause_all_resumes_paused_tasks() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let pause_req = JsonRpcRequest::new("aria2.pause", serde_json::json!([gid])).with_id(2);
        engine.handle_request(&pause_req).await;

        let unpause_req = JsonRpcRequest::new("aria2.unpauseAll", serde_json::json!([])).with_id(3);
        let unpause_resp = engine.handle_request(&unpause_req).await;
        assert!(unpause_resp.is_success());

        let tell_req = JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid])).with_id(4);
        let tell_resp = engine.handle_request(&tell_req).await;
        assert!(tell_resp.is_success());
        let status: StatusInfo = serde_json::from_value(tell_resp.result.unwrap()).unwrap();
        assert_eq!(status.status, DownloadStatus::Active, "Task should be Active after unpauseAll");
    }

    #[tokio::test]
    async fn test_change_uri_adds_uris() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/original.iso"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let change_req = JsonRpcRequest::new(
            "aria2.changeUri",
            serde_json::json!([gid, 0, null, ["http://mirror1.com/file.iso", "http://mirror2.com/file.iso"]]),
        ).with_id(2);
        let change_resp = engine.handle_request(&change_req).await;
        assert!(change_resp.is_success(), "changeUri should succeed");

        let result = change_resp.result.unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr[0], gid, "First element of result should be gid");
        assert_eq!(arr[1], 0, "Second element should be 0 (no file index change)");
    }

    #[tokio::test]
    async fn test_download_resume_event() {
        let event = DownloadEvent::download_resume("gid-resume-001");
        assert_eq!(event.event_type().unwrap(), EventType::DownloadResume);
        assert_eq!(event.method(), "aria2.onDownloadResume");
        let json = event.to_json().unwrap();
        assert!(json.contains("\"method\":\"aria2.onDownloadResume\""));
        assert!(json.contains("\"gid\":\"gid-resume-001\""));
    }

    #[tokio::test]
    async fn test_change_option_rejects_unknown_key() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let req = JsonRpcRequest::new(
            "aria2.changeOption",
            serde_json::json!([gid, {"totally-invalid-option": 42}]),
        ).with_id(2);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_error(), "changeOption with unknown key should fail");
        assert_eq!(resp.error.unwrap().code, -32602, "error code should be InvalidParams (-32602)");
    }

    #[tokio::test]
    async fn test_change_option_accepts_valid_keys() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let valid_changes = serde_json::json!({
            "max-download-limit": 1048576,
            "split": 5,
            "dir": "/tmp/downloads"
        });
        let req = JsonRpcRequest::new(
            "aria2.changeOption",
            serde_json::json!([gid, valid_changes]),
        ).with_id(2);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "changeOption with valid keys should succeed");

        let get_req = JsonRpcRequest::new("aria2.getOption", serde_json::json!([gid])).with_id(3);
        let get_resp = engine.handle_request(&get_req).await;
        assert!(get_resp.is_success());
        let opts: HashMap<String, serde_json::Value> = serde_json::from_value(get_resp.result.unwrap()).unwrap();
        assert!(opts.get("max-download-limit").is_some(), "max-download-limit should be stored");
        assert!(opts.get("split").is_some(), "split should be stored");
    }
}
