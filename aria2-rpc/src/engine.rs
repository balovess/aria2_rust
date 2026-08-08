use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};

use super::json_rpc::{JsonRpcRequest, JsonRpcResponse};
use super::rpc_helpers::split_auth_token;
use super::server::{AuthConfig, CorsConfig, RpcAuthMiddleware};
use super::types::{GlobalOptions, SessionInfo, TaskOptions};
use super::websocket::EventPublisher;
use aria2_core::config::OptionRegistry;
use aria2_core::request::request_group_man::RequestGroupMan;

/// Core RPC engine. Download state lives exclusively in `RequestGroupMan` and
/// lifecycle changes are submitted to the core engine command channel.
pub struct RpcEngine {
    pub(crate) global_opts: GlobalOptions,
    pub(crate) user_global_opts: Arc<RwLock<HashMap<String, serde_json::Value>>>,
    pub(crate) task_opts: TaskOptions,
    pub event_publisher: Arc<EventPublisher>,
    pub(crate) auth_middleware: RpcAuthMiddleware,
    pub(crate) group_man: Option<Arc<RwLock<RequestGroupMan>>>,
    pub(crate) engine_cmd_tx:
        Option<mpsc::UnboundedSender<aria2_core::engine::engine_command::EngineCommand>>,
    pub(crate) session_info: SessionInfo,
    pub(crate) save_session_path: Option<std::path::PathBuf>,
}

impl RpcEngine {
    /// Create a new RpcEngine test fixture with private core dependencies.
    /// Production callers should wire shared dependencies with the builder methods.
    pub fn new() -> Self {
        let (engine_cmd_tx, engine_cmd_rx) = mpsc::unbounded_channel();
        std::mem::forget(engine_cmd_rx);
        Self::wired(Arc::new(RwLock::new(RequestGroupMan::new())), engine_cmd_tx)
    }

    pub fn wired(
        group_man: Arc<RwLock<RequestGroupMan>>,
        engine_cmd_tx: mpsc::UnboundedSender<aria2_core::engine::engine_command::EngineCommand>,
    ) -> Self {
        // Initialize global options from the registry.
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
            global_opts: Arc::new(RwLock::new(defaults)),
            // Keep user-provided global options separate from registry defaults.
            user_global_opts: Arc::new(RwLock::new(HashMap::new())),
            task_opts: Arc::new(RwLock::new(HashMap::new())),
            event_publisher: Arc::new(EventPublisher::default()),
            auth_middleware: RpcAuthMiddleware::default(),
            group_man: Some(group_man),
            engine_cmd_tx: Some(engine_cmd_tx),
            session_info: SessionInfo::new(),
            save_session_path: None,
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

    /// Chainable builder method to set the EngineCommand channel sender.
    /// When set, RPC handlers send structured lifecycle commands (AddDownload,
    /// RemoveDownload, Pause, etc.) to the engine loop.
    pub fn with_engine_cmd_tx(
        mut self,
        tx: mpsc::UnboundedSender<aria2_core::engine::engine_command::EngineCommand>,
    ) -> Self {
        self.engine_cmd_tx = Some(tx);
        self
    }

    /// Chainable builder method to set the configured `--save-session` path.
    ///
    /// `aria2.saveSession` uses this when the RPC request does not include an
    /// explicit filename, mirroring C++ `SaveSessionRpcMethod` which reads the
    /// engine's `PREF_SAVE_SESSION` option.
    pub fn with_save_session_path(mut self, path: std::path::PathBuf) -> Self {
        self.save_session_path = Some(path);
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
        match self.group_man.as_ref() {
            Some(man) => man.read().await.count(),
            None => 0,
        }
    }

    /// Main request dispatcher - routes RPC methods to their handlers.
    ///
    /// This is the central entry point for all JSON-RPC requests.
    ///
    /// Pipeline:
    /// 1. Split the `"token:xxx"` secret off `params[0]` and validate it
    ///    against the configured `rpc-secret` (if any) via [`RpcAuthMiddleware`].
    /// 2. Route `system.multicall` to [`RpcEngine::handle_multicall`] and every
    ///    other method to [`RpcEngine::dispatch_single`].
    /// 3. Post-process the result into aria2's wire format.
    ///
    /// `system.multicall` is deliberately exempt from step 1's *mandatory*
    /// check: C++ aria2's `SystemMulticallRpcMethod::execute()` overrides the
    /// base `RpcMethod::execute()` and therefore never calls `authorize()` on
    /// the multicall envelope — each sub-call authorizes itself instead.
    /// AriaNg / webui-aria2 depend on this, since they put the secret into
    /// every sub-call's `params[0]` and never into the envelope. An envelope
    /// token that *is* present is still validated here and additionally used
    /// as the fallback secret for sub-calls that omit their own.
    pub async fn handle_request(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let is_multicall = req.method == "system.multicall";

        // Authenticate: extract token from params and validate.
        // Supports both array-style ("token:xxx" as first param element)
        // and object-style ({"token": "xxx"}) params for backward compatibility.
        let (token, stripped_params) = split_auth_token(&req.params);
        if (!is_multicall || token.is_some())
            && let Err(auth_err) = self.auth_middleware.validate(token.as_deref())
        {
            return auth_err.into_response(req.id.clone());
        }

        // Dispatch against the token-stripped params so method handlers never
        // see the secret — matching C++ aria2's authorize() behaviour which
        // pops the token from the params array before dispatching.
        let stripped_req;
        let dispatch_req = match stripped_params {
            Some(params) => {
                stripped_req = JsonRpcRequest {
                    version: req.version.clone(),
                    method: req.method.clone(),
                    params,
                    id: req.id.clone(),
                };
                &stripped_req
            }
            None => req,
        };

        let resp = if is_multicall {
            self.handle_multicall(dispatch_req, token.as_deref())
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone()))
        } else {
            self.dispatch_single(dispatch_req).await
        };

        resp
    }

    /// Dispatch one already-authenticated, token-stripped JSON-RPC request.
    ///
    /// This is *the* method table: every registered `aria2.*` method plus
    /// `system.listMethods` / `system.listNotifications` is routed from here.
    /// Both [`RpcEngine::handle_request`] (single calls) and
    /// [`RpcEngine::handle_multicall`] (batched sub-calls) go through it, so
    /// the batched API surface can never drift from the single-call one.
    ///
    /// `system.multicall` is intentionally *not* dispatched here — aria2
    /// forbids nesting a multicall inside a multicall (C++
    /// `SystemMulticallRpcMethod::execute` rejects it with "Recursive
    /// system.multicall forbidden."). Excluding it also keeps this method
    /// non-recursive, so no `Box::pin` indirection is required.
    ///
    /// The returned response is **not** converted to aria2 wire format; the
    /// caller applies that once to the outermost response.
    pub(crate) async fn dispatch_single(&self, dispatch_req: &JsonRpcRequest) -> JsonRpcResponse {
        let id = dispatch_req.id.clone().unwrap_or(serde_json::Value::Null);

        match dispatch_req.method.as_str() {
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
            "system.listMethods" => self
                .handle_list_methods(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            "system.listNotifications" => self
                .handle_list_notifications(dispatch_req)
                .await
                .unwrap_or_else(|e| e.into_response(dispatch_req.id.clone())),
            // Guard against recursion: a multicall may not contain a multicall.
            // Mirrors C++ SystemMulticallRpcMethod::execute()'s
            // "Recursive system.multicall forbidden." branch.
            "system.multicall" => JsonRpcResponse::error(
                id,
                -32600,
                "Nested system.multicall is not supported".to_string(),
            ),
            _ => JsonRpcResponse::error(id, 1, format!("No such method: {}", dispatch_req.method)),
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
        let err = resp.error.unwrap();
        assert_eq!(err.code, 1);
        assert!(err.message.contains("No such method"));
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
    async fn test_change_global_option_max_concurrent_emits_command() {
        use aria2_core::engine::engine_command::EngineCommand;

        let engine = RpcEngine::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<EngineCommand>();
        let engine = engine.with_engine_cmd_tx(tx);

        let set_req = JsonRpcRequest::new(
            "aria2.changeGlobalOption",
            serde_json::json!([{"max-concurrent-downloads": "3"}]),
        )
        .with_id(1);
        let resp = engine.handle_request(&set_req).await;
        assert!(resp.is_success());

        // The engine must receive SetMaxConcurrent so the slot limit is
        // applied live (previously the value was only stored for display).
        match rx.try_recv() {
            Ok(EngineCommand::SetMaxConcurrent { max }) => assert_eq!(max, 3),
            other => panic!("expected SetMaxConcurrent command, got {:?}", other.is_ok()),
        }
    }

    #[tokio::test]
    async fn test_change_global_option_rate_limit_emits_command() {
        use aria2_core::engine::engine_command::EngineCommand;

        let engine = RpcEngine::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<EngineCommand>();
        let engine = engine.with_engine_cmd_tx(tx);

        let set_req = JsonRpcRequest::new(
            "aria2.changeGlobalOption",
            serde_json::json!([
                {
                    "max-overall-download-limit": "2M",
                    "max-overall-upload-limit": "0"
                }
            ]),
        )
        .with_id(1);
        let resp = engine.handle_request(&set_req).await;
        assert!(resp.is_success());

        match rx.try_recv() {
            Ok(EngineCommand::SetGlobalRateLimit {
                download_limit,
                upload_limit,
            }) => {
                assert_eq!(download_limit, Some(2 * 1024 * 1024));
                assert_eq!(upload_limit, None);
            }
            other => panic!(
                "expected SetGlobalRateLimit command, got {:?}",
                other.is_ok()
            ),
        }
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
