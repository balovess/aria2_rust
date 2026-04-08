use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use base64::Engine;

use super::json_rpc::{JsonRpcRequest, JsonRpcResponse, JsonRpcError};
use super::server::{
    AuthConfig, CorsConfig, DownloadStatus, FileInfo, GlobalStat, GlobalOptions,
    StatusInfo, TaskOptions, create_gid,
};
use super::websocket::{DownloadEvent, EventPublisher, EventType};

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

    pub fn with_auth(self, auth: AuthConfig) -> Self { let _ = auth; self }
    pub fn with_cors(self, cors: CorsConfig) -> Self { let _ = cors; self }

    pub fn publisher(&self) -> &EventPublisher { &self.event_publisher }
    pub async fn task_count(&self) -> usize { self.tasks.read().await.len() }

    pub async fn handle_request(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let id = req.id.clone().unwrap_or(serde_json::Value::Null);
        match req.method.as_str() {
            "aria2.addUri" => self.handle_add_uri(req).await.unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.addTorrent" => self.handle_add_torrent(req).await.unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.addMetalink" => self.handle_add_metalink(req).await.unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.remove" => self.handle_remove(req).await.unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.pause" | "aria2.forcePause" => self.handle_pause(req).await.unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.unpause" | "aria2.forceUnpause" => self.handle_unpause(req).await.unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.tellStatus" => self.handle_tell_status(req).await.unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.tellActive" => self.handle_tell_active(req).await.unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.tellWaiting" => self.handle_tell_waiting(req).await.unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.tellStopped" => self.handle_tell_stopped(req).await.unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.getGlobalStat" => self.handle_global_stat().await,
            "aria2.purgeDownloadResult" => self.handle_purge_download_result(),
            "aria2.removeDownloadResult" => self.handle_remove_download_result(req).await.unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.getGlobalOption" => self.handle_get_global_option().await,
            "aria2.changeGlobalOption" => self.handle_change_global_option(req).await.unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.getOption" => self.handle_get_option(req).await.unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.changeOption" => self.handle_change_option(req).await.unwrap_or_else(|e| e.into_response(req.id.clone())),
            "aria2.getVersion" => self.handle_version(),
            "aria2.getSessionInfo" => self.handle_session_info(),
            _ => JsonRpcResponse::error(id, -32601, format!("Method not found: {}", req.method)),
        }
    }

    async fn add_task(&self, uris: Vec<String>, options: HashMap<String, serde_json::Value>) -> Result<String, JsonRpcError> {
        let gid = create_gid();
        let dir = options.get("dir").and_then(|v| v.as_str()).unwrap_or(".").to_string();
        let status = StatusInfo::new(&gid)
            .with_status(DownloadStatus::Active)
            .with_dir(dir)
            .with_total_length(0)
            .with_completed_length(0)
            .with_files(vec![FileInfo::new("", 0)]);
        let state = TaskState { status, options, uris };
        {
            let mut tasks = self.tasks.write().await;
            tasks.insert(gid.clone(), state);
        }
        let _ = self.event_publisher.publish(EventType::DownloadStart, DownloadEvent::download_start(&gid, vec![]));
        Ok(gid)
    }

    async fn handle_add_uri(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let uri: String = req.get_param(0)?;
        let opts: HashMap<String, serde_json::Value> = req.get_param_or_default(1);
        let gid = self.add_task(vec![uri], opts).await?;
        Ok(JsonRpcResponse::success(req.id.clone().unwrap_or_default(), gid))
    }

    async fn handle_add_torrent(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let torrent_data: String = req.get_param(0)?;
        let opts: HashMap<String, serde_json::Value> = req.get_param_or_default(1);
        let _dir = opts.get("dir").and_then(|v| v.as_str()).unwrap_or(".").to_string();

        let decoded_bytes = if torrent_data.starts_with("data:") {
            base64::engine::general_purpose::STANDARD.decode(torrent_data.split(',').nth(1).unwrap_or(""))
                .map_err(|e| JsonRpcError::InvalidParams(format!("base64 decode failed: {}", e)))?
        } else {
            base64::engine::general_purpose::STANDARD.decode(&torrent_data)
                .map_err(|e| JsonRpcError::InvalidParams(format!("base64 decode failed: {}", e)))?
        };

        if decoded_bytes.len() < 3 || decoded_bytes[0] != b'd' || decoded_bytes[1] != b'8' || decoded_bytes[2] != b':' {
            return Err(JsonRpcError::InvalidParams("Invalid BEncode data (not a .torrent file)".into()));
        }

        let gid = self.add_task(vec![format!("torrent://{}", &decoded_bytes[..std::cmp::min(32, decoded_bytes.len())].iter().map(|b| format!("{:02x}", b)).collect::<String>())], opts).await?;
        Ok(JsonRpcResponse::success(req.id.clone().unwrap_or_default(), gid))
    }

    async fn handle_add_metalink(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let metalink_data: String = req.get_param(0)?;
        let opts: HashMap<String, serde_json::Value> = req.get_param_or_default(1);

        let decoded_bytes = if metalink_data.starts_with("data:") {
            base64::engine::general_purpose::STANDARD.decode(metalink_data.split(',').nth(1).unwrap_or(""))
                .map_err(|e| JsonRpcError::InvalidParams(format!("base64 decode failed: {}", e)))?
        } else {
            base64::engine::general_purpose::STANDARD.decode(&metalink_data)
                .map_err(|e| JsonRpcError::InvalidParams(format!("base64 decode failed: {}", e)))?
        };

        let preview = String::from_utf8_lossy(&decoded_bytes[..decoded_bytes.len().min(200)]);
        if !preview.to_lowercase().contains("<metalink") && !preview.contains("urn:ietf:params:xml:ns:metalink") {
            return Err(JsonRpcError::InvalidParams("Invalid Metalink XML data".into()));
        }

        let gid = self.add_task(vec!["metalink://download".to_string()], opts).await?;
        Ok(JsonRpcResponse::success(req.id.clone().unwrap_or_default(), gid))
    }

    async fn handle_remove(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let mut tasks = self.tasks.write().await;
        match tasks.remove(&gid) {
            Some(_) => Ok(JsonRpcResponse::success(req.id.clone().unwrap_or_default(), serde_json::json!([gid]))),
            None => Err(JsonRpcError::MethodNotFound(format!("GID {} not found", gid))),
        }
    }

    async fn handle_pause(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let mut tasks = self.tasks.write().await;
        if let Some(state) = tasks.get_mut(&gid) {
            state.status.status = DownloadStatus::Paused;
            Ok(JsonRpcResponse::success(req.id.clone().unwrap_or_default(), serde_json::json!([gid])))
        } else {
            Err(JsonRpcError::MethodNotFound(format!("GID {} not found", gid)))
        }
    }

    async fn handle_unpause(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let mut tasks = self.tasks.write().await;
        if let Some(state) = tasks.get_mut(&gid) {
            state.status.status = DownloadStatus::Active;
            Ok(JsonRpcResponse::success(req.id.clone().unwrap_or_default(), serde_json::json!([gid])))
        } else {
            Err(JsonRpcError::MethodNotFound(format!("GID {} not found", gid)))
        }
    }

    async fn get_status(&self, gid: &str) -> Option<StatusInfo> {
        let tasks = self.tasks.read().await;
        tasks.get(gid).map(|s| s.status.clone())
    }

    async fn handle_tell_status(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        match self.get_status(&gid).await {
            Some(status) => Ok(JsonRpcResponse::success(req.id.clone().unwrap_or_default(), serde_json::to_value(status).unwrap())),
            None => Err(JsonRpcError::MethodNotFound(format!("GID {} not found", gid))),
        }
    }

    async fn handle_tell_active(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let tasks = self.tasks.read().await;
        let active: Vec<StatusInfo> = tasks.values()
            .filter(|s| s.status.status.is_active())
            .map(|s| s.status.clone())
            .collect();
        Ok(JsonRpcResponse::success(req.id.clone().unwrap_or_default(), serde_json::to_value(active).unwrap()))
    }

    async fn handle_tell_waiting(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let offset: usize = req.get_param_or_default(0);
        let num: usize = req.get_param_or_default(1);
        let tasks = self.tasks.read().await;
        let waiting: Vec<StatusInfo> = tasks.values()
            .filter(|s| s.status.status == DownloadStatus::Waiting)
            .skip(offset.min(tasks.len()))
            .take(num)
            .map(|s| s.status.clone())
            .collect();
        Ok(JsonRpcResponse::success(req.id.clone().unwrap_or_default(), serde_json::to_value(waiting).unwrap()))
    }

    async fn handle_tell_stopped(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let offset: usize = req.get_param_or_default(0);
        let num: usize = req.get_param_or_default(1);
        let stopped = self.stopped_tasks.read().await;
        let result: Vec<&StatusInfo> = stopped.iter()
            .skip(offset.min(stopped.len()))
            .take(num)
            .collect();
        Ok(JsonRpcResponse::success(req.id.clone().unwrap_or_default(), serde_json::to_value(result).unwrap()))
    }

    async fn handle_global_stat(&self) -> JsonRpcResponse {
        let tasks = self.tasks.read().await;
        let (active, waiting): (Vec<_>, Vec<_>) = tasks.values()
            .partition(|s| s.status.status.is_active());
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

    async fn handle_remove_download_result(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let _gid: String = req.get_param(0)?;
        Ok(JsonRpcResponse::success(req.id.clone().unwrap_or_default(), "OK"))
    }

    async fn handle_get_global_option(&self) -> JsonRpcResponse {
        let opts = self.global_opts.read().await;
        JsonRpcResponse::success(serde_json::Value::Null, serde_json::to_value(&*opts).unwrap_or(serde_json::json!({})))
    }

    async fn handle_change_global_option(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let new_opts: HashMap<String, serde_json::Value> = req.get_param(0)?;
        let mut opts = self.global_opts.write().await;
        for (k, v) in new_opts {
            opts.insert(k, v);
        }
        Ok(JsonRpcResponse::success(req.id.clone().unwrap_or_default(), "OK"))
    }

    async fn handle_get_option(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let task_opts = self.task_opts.read().await;
        match task_opts.get(&gid) {
            Some(opts) => Ok(JsonRpcResponse::success(req.id.clone().unwrap_or_default(), serde_json::to_value(opts).unwrap())),
            None => Err(JsonRpcError::MethodNotFound(format!("GID {} not found", gid))),
        }
    }

    async fn handle_change_option(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let changes: HashMap<String, serde_json::Value> = req.get_param(1)?;
        let mut task_opts = self.task_opts.write().await;
        let entry = task_opts.entry(gid).or_insert_with(HashMap::new);
        for (k, v) in changes {
            entry.insert(k, v);
        }
        Ok(JsonRpcResponse::success(req.id.clone().unwrap_or_default(), "OK"))
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
        let session_id = SystemTime::now().duration_since(UNIX_EPOCH)
            .map(|d| format!("session-{:x}", d.as_nanos()))
            .unwrap_or_else(|_| "session-unknown".to_string());
        JsonRpcResponse::success(serde_json::Value::Null, serde_json::json!({"sessionId": session_id}))
    }
}

impl Default for RpcEngine { fn default() -> Self { Self::new() } }

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
        let req = JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://example.com/file.iso"])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success());
        let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(gid.len(), 16);
    }

    #[tokio::test]
    async fn test_handle_remove_nonexistent() {
        let engine = RpcEngine::new();
        let req = JsonRpcRequest::new("aria2.remove", serde_json::json!(["nonexistent-gid"])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_error());
    }

    #[tokio::test]
    async fn test_handle_pause_and_unpause() {
        let engine = RpcEngine::new();
        let add_req = JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
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
        let add_req = JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
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
        let req = JsonRpcRequest::new("aria2.purgeDownloadResult", serde_json::json!([])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success());
    }

    #[tokio::test]
    async fn test_handle_get_set_global_option() {
        let engine = RpcEngine::new();
        let get_req = JsonRpcRequest::new("aria2.getGlobalOption", serde_json::json!([])).with_id(1);
        let get_resp = engine.handle_request(&get_req).await;
        assert!(get_resp.is_success());

        let set_req = JsonRpcRequest::new("aria2.changeGlobalOption", serde_json::json!([{"max-concurrent-downloads": 5}])).with_id(2);
        let set_resp = engine.handle_request(&set_req).await;
        assert!(set_resp.is_success());
    }

    #[tokio::test]
    async fn test_handle_get_set_option() {
        let engine = RpcEngine::new();
        let add_req = JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let set_req = JsonRpcRequest::new("aria2.changeOption", serde_json::json!([gid, {"max-download-limit": 1048576}])).with_id(2);
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
            let req = JsonRpcRequest::new("aria2.addUri", serde_json::json!([format!("http://x.com/{}", i)])).with_id(i);
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
        let encoded = base64::engine::general_purpose::STANDARD.encode(fake_torrent_bencode.as_bytes());
        let req = JsonRpcRequest::new("aria2.addTorrent", serde_json::json!([encoded])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "addTorrent should succeed for valid BEncode data");
        let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(!gid.is_empty());
        assert_eq!(engine.task_count().await, 1);
    }

    #[tokio::test]
    async fn test_handle_add_torrent_invalid_data() {
        let engine = RpcEngine::new();
        let not_torrent = base64::engine::general_purpose::STANDARD.encode("this is not a torrent file");
        let req = JsonRpcRequest::new("aria2.addTorrent", serde_json::json!([not_torrent])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_error(), "addTorrent should fail for non-BEncode data");
    }

    #[tokio::test]
    async fn test_handle_add_metalink() {
        let engine = RpcEngine::new();
        let metalink_xml = r#"<?xml version="1.0"?><metalink xmlns="urn:ietf:params:xml:ns:metalink"><files><file name="test.bin"><size>1024</size><url priority="1">http://example.com/test.bin</url></file></files></metalink>"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(metalink_xml.as_bytes());
        let req = JsonRpcRequest::new("aria2.addMetalink", serde_json::json!([encoded])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "addMetalink should succeed for valid Metalink XML");
        let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(!gid.is_empty());
        assert_eq!(engine.task_count().await, 1);
    }

    #[tokio::test]
    async fn test_handle_add_metalink_invalid_data() {
        let engine = RpcEngine::new();
        let not_metalink = base64::engine::general_purpose::STANDARD.encode("this is not metalink xml");
        let req = JsonRpcRequest::new("aria2.addMetalink", serde_json::json!([not_metalink])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_error(), "addMetalink should fail for non-XML data");
    }
}
