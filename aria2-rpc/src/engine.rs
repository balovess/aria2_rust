//! RPC wire engine.
//!
//! This module owns authentication, JSON-RPC dispatch, multicall semantics,
//! batch ordering, and WebSocket event publication. Domain state and all
//! download operations live behind [`RpcBackend`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt;
use tokio::sync::Notify;

use crate::backend::{
    BackendMetadata, BackendReadSnapshot, BackendRequest, BackendResponse, RpcBackend,
    UnsupportedBackend,
};
use crate::handlers::{self, backend_error};
use crate::json_rpc::{JsonRpcRequest, JsonRpcResponse, JsonRpcWireEntry};
use crate::server::{AuthConfig, CorsConfig, RpcAuthMiddleware};
use crate::types::SessionInfo;
use crate::websocket::EventPublisher;

pub(crate) fn rpc_method_requires_auth(method: &str) -> bool {
    !matches!(method, "system.listMethods" | "system.listNotifications")
}

pub(crate) const MAX_RPC_BATCH_CONCURRENCY: usize = 64;

pub(crate) fn rpc_method_is_read_only(method: &str) -> bool {
    matches!(
        method,
        "aria2.tellStatus"
            | "aria2.tellActive"
            | "aria2.tellWaiting"
            | "aria2.tellStopped"
            | "aria2.getGlobalStat"
            | "aria2.getUris"
            | "aria2.getFiles"
            | "aria2.getServers"
            | "aria2.getGlobalOption"
            | "aria2.getOption"
            | "aria2.getPeers"
            | "aria2.getVersion"
            | "aria2.getSessionInfo"
            | "system.listMethods"
            | "system.listNotifications"
    )
}

pub(crate) fn rpc_method_is_mutating(method: &str) -> bool {
    matches!(
        method,
        "aria2.addUri"
            | "aria2.addTorrent"
            | "aria2.addMetalink"
            | "aria2.remove"
            | "aria2.pause"
            | "aria2.forcePause"
            | "aria2.unpause"
            | "aria2.purgeDownloadResult"
            | "aria2.removeDownloadResult"
            | "aria2.changeGlobalOption"
            | "aria2.changeOption"
            | "aria2.pauseAll"
            | "aria2.forcePauseAll"
            | "aria2.unpauseAll"
            | "aria2.changeUri"
            | "aria2.saveSession"
            | "aria2.changePosition"
            | "aria2.forceRemove"
            | "aria2.shutdown"
            | "aria2.forceShutdown"
    )
}

pub(crate) fn rpc_method_uses_read_snapshot(method: &str) -> bool {
    matches!(
        method,
        "aria2.tellActive" | "aria2.tellWaiting" | "aria2.tellStopped" | "aria2.getGlobalStat"
    )
}

fn multicall_requires_mutation(params: &serde_json::Value) -> bool {
    let Some(calls) = params
        .as_array()
        .and_then(|params| params.first())
        .and_then(serde_json::Value::as_array)
    else {
        return true;
    };
    calls.iter().any(|call| {
        call.get("methodName")
            .and_then(serde_json::Value::as_str)
            .is_none_or(rpc_method_is_mutating)
    })
}

fn read_run_needs_snapshot(entries: &[JsonRpcWireEntry]) -> bool {
    entries
        .iter()
        .filter_map(|entry| match entry {
            JsonRpcWireEntry::Request(request) => Some(request.method.as_str()),
            JsonRpcWireEntry::Error(_) => None,
        })
        .filter(|method| rpc_method_uses_read_snapshot(method))
        .count()
        > 1
}

async fn dispatch_wire_entry(
    engine: &RpcEngine,
    entry: JsonRpcWireEntry,
    snapshot: Option<Arc<BackendReadSnapshot>>,
) -> JsonRpcResponse {
    match entry {
        JsonRpcWireEntry::Request(request) => {
            engine
                .handle_request_owned_with_snapshot(request, snapshot)
                .await
        }
        JsonRpcWireEntry::Error(response) => response,
    }
}

async fn dispatch_read_only_run(
    engine: &RpcEngine,
    entries: Vec<JsonRpcWireEntry>,
) -> Vec<JsonRpcResponse> {
    let snapshot = if read_run_needs_snapshot(&entries) {
        match engine.backend.capture_read_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return entries
                    .into_iter()
                    .map(|entry| {
                        let id = match entry {
                            JsonRpcWireEntry::Request(request) => request.id,
                            JsonRpcWireEntry::Error(response) => Some(response.id),
                        };
                        backend_error(error.clone()).into_response(id)
                    })
                    .collect();
            }
        }
    } else {
        None
    };

    futures::stream::iter(entries)
        .map(|entry| dispatch_wire_entry(engine, entry, snapshot.clone()))
        .buffered(MAX_RPC_BATCH_CONCURRENCY)
        .collect()
        .await
}

pub(crate) async fn dispatch_wire_entries(
    engine: &RpcEngine,
    entries: Vec<JsonRpcWireEntry>,
) -> Vec<JsonRpcResponse> {
    let mut responses = Vec::with_capacity(entries.len());
    let mut read_only_run = Vec::new();
    for entry in entries {
        let read_only = match &entry {
            JsonRpcWireEntry::Request(request) => rpc_method_is_read_only(&request.method),
            JsonRpcWireEntry::Error(_) => true,
        };
        if read_only {
            read_only_run.push(entry);
        } else {
            if !read_only_run.is_empty() {
                responses.extend(
                    dispatch_read_only_run(engine, std::mem::take(&mut read_only_run)).await,
                );
            }
            responses.push(dispatch_wire_entry(engine, entry, None).await);
        }
    }
    if !read_only_run.is_empty() {
        responses.extend(dispatch_read_only_run(engine, read_only_run).await);
    }
    responses
}

pub struct RpcEngine {
    pub(crate) backend: Arc<dyn RpcBackend>,
    pub(crate) metadata: BackendMetadata,
    pub event_publisher: Arc<EventPublisher>,
    pub(crate) auth_middleware: RpcAuthMiddleware,
    pub(crate) mutation_gate: Arc<tokio::sync::Mutex<()>>,
    mutation_ticket: AtomicU64,
    mutation_turn: AtomicU64,
    mutation_notify: Notify,
    pub(crate) session_info: SessionInfo,
}

struct MutationTurn<'a> {
    engine: &'a RpcEngine,
    ticket: u64,
    _gate: tokio::sync::MutexGuard<'a, ()>,
}

impl Drop for MutationTurn<'_> {
    fn drop(&mut self) {
        let previous = self
            .engine
            .mutation_turn
            .swap(self.ticket + 1, Ordering::Release);
        debug_assert_eq!(previous, self.ticket);
        self.engine.mutation_notify.notify_waiters();
    }
}

impl RpcEngine {
    pub fn new() -> Self {
        Self::with_backend(Arc::new(UnsupportedBackend))
    }

    pub fn with_backend(backend: Arc<dyn RpcBackend>) -> Self {
        let metadata = backend.metadata();
        Self {
            backend,
            metadata,
            event_publisher: Arc::new(EventPublisher::default()),
            auth_middleware: RpcAuthMiddleware::default(),
            mutation_gate: Arc::new(tokio::sync::Mutex::new(())),
            mutation_ticket: AtomicU64::new(0),
            mutation_turn: AtomicU64::new(0),
            mutation_notify: Notify::new(),
            session_info: SessionInfo::new(),
        }
    }

    pub fn with_product_version(mut self, version: impl Into<String>) -> Self {
        self.metadata.product_version = version.into();
        self
    }

    pub fn with_auth(self, auth: AuthConfig) -> Self {
        if let Some(token) = auth.token {
            return self.with_auth_middleware(RpcAuthMiddleware::new(&token));
        }
        self
    }

    pub fn with_auth_middleware(mut self, middleware: RpcAuthMiddleware) -> Self {
        self.auth_middleware = middleware;
        self
    }

    pub fn with_cors(self, _cors: CorsConfig) -> Self {
        self
    }

    pub fn publisher(&self) -> &EventPublisher {
        &self.event_publisher
    }

    pub async fn task_count(&self) -> usize {
        self.backend.task_count().await
    }

    fn reserve_mutation_ticket(&self) -> u64 {
        self.mutation_ticket.fetch_add(1, Ordering::Relaxed)
    }

    async fn acquire_mutation_turn(&self, ticket: u64) -> MutationTurn<'_> {
        loop {
            let notified = self.mutation_notify.notified();
            if self.mutation_turn.load(Ordering::Acquire) == ticket {
                let gate = self.mutation_gate.lock().await;
                if self.mutation_turn.load(Ordering::Acquire) == ticket {
                    return MutationTurn {
                        engine: self,
                        ticket,
                        _gate: gate,
                    };
                }
            }
            notified.await;
        }
    }

    async fn run_mutation<F, T>(&self, ticket: u64, operation: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        let _turn = self.acquire_mutation_turn(ticket).await;
        operation.await
    }

    pub async fn handle_request(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        self.handle_request_owned(req.clone()).await
    }

    pub(crate) async fn handle_request_owned(&self, req: JsonRpcRequest) -> JsonRpcResponse {
        self.handle_request_owned_with_snapshot(req, None).await
    }

    async fn handle_request_owned_with_snapshot(
        &self,
        req: JsonRpcRequest,
        snapshot: Option<Arc<BackendReadSnapshot>>,
    ) -> JsonRpcResponse {
        let request_id = req.id.clone();
        if req.method == "system.multicall" {
            if multicall_requires_mutation(&req.params) {
                let ticket = self.reserve_mutation_ticket();
                return self
                    .run_mutation(ticket, self.handle_multicall(req, snapshot))
                    .await
                    .unwrap_or_else(|error| error.into_response(request_id));
            }
            return self
                .handle_multicall(req, snapshot)
                .await
                .unwrap_or_else(|error| error.into_response(request_id));
        }

        let (token, params) = crate::rpc_helpers::split_auth_token_owned(req.params);
        if rpc_method_requires_auth(&req.method)
            && let Err(error) = self.auth_middleware.validate(token.as_deref())
        {
            return error.into_response(request_id);
        }
        let dispatch = JsonRpcRequest {
            version: req.version,
            method: req.method,
            params,
            id: req.id,
        };
        let is_mutating = rpc_method_is_mutating(&dispatch.method);
        if is_mutating {
            let ticket = self.reserve_mutation_ticket();
            self.run_mutation(ticket, self.execute_request(dispatch, snapshot))
                .await
                .unwrap_or_else(|error| error.into_response(request_id))
        } else {
            self.execute_request(dispatch, snapshot)
                .await
                .unwrap_or_else(|error| error.into_response(request_id))
        }
    }

    async fn execute_request(
        &self,
        mut req: JsonRpcRequest,
        snapshot: Option<Arc<BackendReadSnapshot>>,
    ) -> Result<JsonRpcResponse, crate::json_rpc::JsonRpcError> {
        let id = req.id.clone().unwrap_or(serde_json::Value::Null);
        let request = match req.method.as_str() {
            "aria2.addUri" => handlers::task::parse_add_uri(&mut req),
            "aria2.addTorrent" => handlers::task::parse_add_torrent(&mut req),
            "aria2.addMetalink" => handlers::task::parse_add_metalink(&mut req),
            "aria2.remove" => handlers::task::parse_remove(&mut req),
            "aria2.pause" => handlers::task::parse_pause(&mut req),
            "aria2.forcePause" => handlers::task::parse_force_pause(&mut req),
            "aria2.unpause" => handlers::task::parse_unpause(&mut req),
            "aria2.tellStatus" => handlers::task::parse_tell_status(&mut req),
            "aria2.tellActive" => handlers::status::parse_tell_active(&mut req),
            "aria2.tellWaiting" => handlers::status::parse_tell_waiting(&mut req),
            "aria2.tellStopped" => handlers::status::parse_tell_stopped(&mut req),
            "aria2.getGlobalStat" => Ok(BackendRequest::GetGlobalStat),
            "aria2.getUris" => handlers::bittorrent::parse_get_uris(&mut req),
            "aria2.getFiles" => handlers::bittorrent::parse_get_files(&mut req),
            "aria2.getServers" => handlers::bittorrent::parse_get_servers(&mut req),
            "aria2.purgeDownloadResult" => {
                Ok(handlers::bittorrent::parse_purge_download_result(&mut req))
            }
            "aria2.removeDownloadResult" => {
                handlers::bittorrent::parse_remove_download_result(&mut req)
            }
            "aria2.getGlobalOption" => Ok(handlers::options::parse_get_global_option(&mut req)),
            "aria2.changeGlobalOption" => handlers::options::parse_change_global_option(&mut req),
            "aria2.getOption" => handlers::options::parse_get_option(&mut req),
            "aria2.changeOption" => handlers::options::parse_change_option(&mut req),
            "aria2.getPeers" => handlers::bittorrent::parse_get_peers(&mut req),
            "aria2.pauseAll" => Ok(handlers::bittorrent::parse_pause_all(&mut req)),
            "aria2.forcePauseAll" => Ok(handlers::bittorrent::parse_force_pause_all(&mut req)),
            "aria2.unpauseAll" => Ok(handlers::bittorrent::parse_unpause_all(&mut req)),
            "aria2.changeUri" => handlers::task::parse_change_uri(&mut req),
            "aria2.saveSession" => Ok(handlers::task::parse_save_session(&mut req)),
            "aria2.changePosition" => handlers::task::parse_change_position(&mut req),
            "aria2.forceRemove" => handlers::task::parse_force_remove(&mut req),
            "aria2.shutdown" => Ok(handlers::task::parse_shutdown(&mut req, false)),
            "aria2.forceShutdown" => Ok(handlers::task::parse_shutdown(&mut req, true)),
            "aria2.getVersion" => {
                return Ok(JsonRpcResponse::success(
                    id,
                    serde_json::json!({
                        "version": self.metadata.product_version,
                        "enabledFeatures": self.metadata.enabled_features,
                    }),
                ));
            }
            "aria2.getSessionInfo" => {
                return Ok(JsonRpcResponse::success(
                    id,
                    self.session_info.to_json_value(),
                ));
            }
            "system.listMethods" => {
                return Ok(JsonRpcResponse::success(
                    id,
                    serde_json::json!(self.metadata.methods),
                ));
            }
            "system.listNotifications" => {
                return Ok(JsonRpcResponse::success(
                    id,
                    serde_json::json!(self.metadata.notifications),
                ));
            }
            "system.multicall" => {
                return Err(crate::json_rpc::JsonRpcError::RpcExecution(
                    "Recursive system.multicall forbidden.".into(),
                ));
            }
            method => {
                return Err(crate::json_rpc::JsonRpcError::RpcExecution(format!(
                    "No such method: {method}"
                )));
            }
        }?;

        let result = self
            .backend
            .execute_with_snapshot(request.clone(), snapshot)
            .await
            .map_err(backend_error)?;
        self.publish_events(&result.events);
        let value = self.response_value(&request, result.response)?;
        Ok(JsonRpcResponse::success(id, value))
    }

    fn publish_events(&self, events: &[crate::backend::BackendEvent]) {
        for event in events {
            let (event_type, notification) = handlers::event_notification(event.clone());
            let _ = self.event_publisher.publish(event_type, notification);
        }
    }

    fn response_value(
        &self,
        request: &BackendRequest,
        response: BackendResponse,
    ) -> Result<serde_json::Value, crate::json_rpc::JsonRpcError> {
        match request {
            BackendRequest::TellStatus { keys, .. }
            | BackendRequest::TellActive { keys }
            | BackendRequest::TellWaiting { keys, .. }
            | BackendRequest::TellStopped { keys, .. } => {
                handlers::status::serialize_status_response(response, keys)
            }
            BackendRequest::ChangeGlobalOption { .. }
            | BackendRequest::GetGlobalOption
            | BackendRequest::GetOption { .. } => match response {
                BackendResponse::Options(options) => {
                    Ok(handlers::options::normalize_options_response(options))
                }
                _ => response.into_json_value().map_err(backend_error),
            },
            _ => response.into_json_value().map_err(backend_error),
        }
    }

    async fn handle_multicall(
        &self,
        req: JsonRpcRequest,
        mut snapshot: Option<Arc<BackendReadSnapshot>>,
    ) -> Result<JsonRpcResponse, crate::json_rpc::JsonRpcError> {
        let id = req.id.clone().unwrap_or(serde_json::Value::Null);
        let calls = match req.params {
            serde_json::Value::Array(mut params) => match params.drain(..).next() {
                Some(serde_json::Value::Array(calls)) => calls,
                Some(_) => {
                    return Err(crate::json_rpc::JsonRpcError::RpcExecution(
                        "The parameter at 0 has wrong type.".into(),
                    ));
                }
                None => {
                    return Err(crate::json_rpc::JsonRpcError::RpcExecution(
                        "The parameter at 0 is required but missing.".into(),
                    ));
                }
            },
            serde_json::Value::Object(mut params) => match params.remove("p0") {
                Some(serde_json::Value::Array(calls)) => calls,
                Some(_) => {
                    return Err(crate::json_rpc::JsonRpcError::RpcExecution(
                        "The parameter at 0 has wrong type.".into(),
                    ));
                }
                None => {
                    return Err(crate::json_rpc::JsonRpcError::RpcExecution(
                        "The parameter at 0 is required but missing.".into(),
                    ));
                }
            },
            _ => {
                return Err(crate::json_rpc::JsonRpcError::RpcExecution(
                    "The parameter at 0 is required but missing.".into(),
                ));
            }
        };

        if snapshot.is_none()
            && !calls.is_empty()
            && !calls.iter().any(|call| {
                call.get("methodName")
                    .and_then(serde_json::Value::as_str)
                    .is_none_or(rpc_method_is_mutating)
            })
        {
            snapshot = self
                .backend
                .capture_read_snapshot()
                .await
                .map_err(backend_error)?;
        }

        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let mut object = match call {
                serde_json::Value::Object(object) => object,
                _ => {
                    results.push(serde_json::json!({
                        "code": 1,
                        "message": "system.multicall expected struct."
                    }));
                    continue;
                }
            };
            let Some(method) = object
                .remove("methodName")
                .and_then(|value| value.as_str().map(str::to_owned))
            else {
                results.push(serde_json::json!({"code": 1, "message": "Missing methodName."}));
                continue;
            };
            if method == "system.multicall" {
                results.push(serde_json::json!({
                    "code": 1,
                    "message": "Recursive system.multicall forbidden."
                }));
                continue;
            }
            let params = match object.remove("params") {
                Some(serde_json::Value::Array(params)) => serde_json::Value::Array(params),
                _ => serde_json::Value::Array(Vec::new()),
            };
            let (token, params) = crate::rpc_helpers::split_auth_token_owned(params);
            if rpc_method_requires_auth(&method)
                && let Err(error) = self.auth_middleware.validate(token.as_deref())
            {
                results.push(serde_json::json!({
                    "code": error.code(),
                    "message": error.message(),
                }));
                continue;
            }
            let sub_request = JsonRpcRequest::new(method, params);
            match self.execute_request(sub_request, snapshot.clone()).await {
                Ok(response) => {
                    if let Some(result) = response.result {
                        results.push(serde_json::json!([result]));
                    } else if let Some(error) = response.error {
                        results.push(serde_json::json!({
                            "code": error.code,
                            "message": error.message,
                        }));
                    }
                }
                Err(error) => results.push(serde_json::json!({
                    "code": error.code(),
                    "message": error.message(),
                })),
            }
        }
        Ok(JsonRpcResponse::success(id, results))
    }
}

impl Default for RpcEngine {
    fn default() -> Self {
        Self::new()
    }
}
