//! RPC server management
//!
//! This module handles RPC server initialization and lifecycle:
//! - HTTP/HTTPS RPC server setup
//! - Authentication configuration
//! - CORS configuration
//! - Shared engine state (RequestGroupMan + command channel)
//! - Bridging `aria2-core` download lifecycle events to the JSON-RPC
//!   WebSocket notification publisher ([`CoreEventBridge`])

use super::App;
use super::startup::StartupPlan;
#[cfg(test)]
use aria2_core::config::OptionValue;
use aria2_core::engine::download_event_hooks::{
    DownloadEvent as CoreDownloadEvent, DownloadEventHooks, DownloadEventListener,
};
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_rpc::engine::RpcEngine;
use aria2_rpc::server::{
    AuthConfig, CorsConfig, RpcAuthMiddleware, RpcServer, ServerConfig, TlsConfig,
};
use aria2_rpc::websocket::{DownloadEvent as RpcDownloadEvent, EventType};
use std::sync::{Arc, Weak};
use tracing::{debug, error, info};

fn rpc_bind_hosts(host: &str, listen_all: bool, disable_ipv6: bool) -> Result<Vec<String>, String> {
    if disable_ipv6 && host.contains(':') {
        return Err(format!(
            "RPC listen address '{}' is IPv6, but IPv6 is disabled",
            host
        ));
    }

    if listen_all {
        return Ok(if disable_ipv6 {
            vec!["0.0.0.0".to_string()]
        } else {
            vec!["0.0.0.0".to_string(), "::".to_string()]
        });
    }

    Ok(match host {
        "127.0.0.1" if !disable_ipv6 => vec!["127.0.0.1".to_string(), "::1".to_string()],
        "0.0.0.0" if !disable_ipv6 => vec!["0.0.0.0".to_string(), "::".to_string()],
        _ => vec![host.to_string()],
    })
}

// ============================================================================
// Core → RPC download event bridge
// ============================================================================

/// Forwards `aria2-core` download lifecycle events to the JSON-RPC WebSocket
/// event publisher.
///
/// # Why this lives in the binary crate
///
/// `aria2-core` must not depend on `aria2-rpc` (the dependency direction is
/// `aria2 → {aria2-core, aria2-rpc}`), so the core event bus only exposes the
/// crate-local [`DownloadEventListener`] trait. The binary crate owns both
/// halves and is therefore the only place where the two can be wired together.
/// This mirrors C++ aria2, where `main()` installs
/// `WebSocketSessionMan` as a `DownloadEventListener` on the `Notifier`
/// singleton (`SingletonHolder<Notifier>`).
///
/// # Which events are forwarded
///
/// Only the three **terminal** events are bridged:
///
/// | core event                  | JSON-RPC notification         |
/// |-----------------------------|-------------------------------|
/// | [`CoreDownloadEvent::Complete`]   | `aria2.onDownloadComplete`    |
/// | [`CoreDownloadEvent::Error`]      | `aria2.onDownloadError`       |
/// | [`CoreDownloadEvent::BtComplete`] | `aria2.onBtDownloadComplete`  |
///
/// `Start` / `Pause` / `Stop` are deliberately **not** forwarded: the RPC
/// handlers for `aria2.addUri`, `aria2.pause`, `aria2.unpause`,
/// `aria2.remove` and friends publish those directly at the moment the client
/// request is serviced. Bridging them here as well would deliver every such
/// notification twice.
///
/// # Lifetime
///
/// The bridge holds a [`Weak`] reference to the [`RpcEngine`]. The core bus is
/// a process-wide singleton with no listener-removal API, so a strong
/// reference would keep the engine (and everything it owns — the group
/// manager, the command channel, all task state) alive for the whole process
/// lifetime even after the RPC server has been torn down. When the upgrade
/// fails the event is silently dropped: there is no publisher left to receive
/// it.
pub struct CoreEventBridge {
    engine: Weak<RpcEngine>,
}

impl CoreEventBridge {
    /// Create a bridge that publishes into `engine`'s event publisher.
    pub fn new(engine: &Arc<RpcEngine>) -> Self {
        Self {
            engine: Arc::downgrade(engine),
        }
    }

    /// Map a core lifecycle event onto its JSON-RPC notification, or `None`
    /// when the event is already published by an RPC handler.
    pub fn map_event(event: CoreDownloadEvent, gid: &str) -> Option<(EventType, RpcDownloadEvent)> {
        match event {
            CoreDownloadEvent::Complete => Some((
                EventType::DownloadComplete,
                RpcDownloadEvent::download_complete(gid),
            )),
            CoreDownloadEvent::Error => Some((
                EventType::DownloadError,
                RpcDownloadEvent::download_error(gid),
            )),
            CoreDownloadEvent::BtComplete => Some((
                EventType::BtDownloadComplete,
                RpcDownloadEvent::bt_download_complete(gid),
            )),
            // Published directly by the RPC handlers — see the type docs.
            CoreDownloadEvent::Start | CoreDownloadEvent::Pause | CoreDownloadEvent::Stop => None,
        }
    }
}

impl DownloadEventListener for CoreEventBridge {
    /// Publish the mapped notification.
    ///
    /// # Contract compliance
    ///
    /// This runs **inline on the thread that performed the download state
    /// transition**, which may still be holding unrelated core locks, and it
    /// may be a plain synchronous (non-async) context. It therefore must not
    /// block, await, or `block_on`. `EventPublisher::publish` is a
    /// `tokio::sync::broadcast::Sender::send`, which is synchronous,
    /// lock-free-ish and never blocks — so it can be called directly.
    ///
    /// The method also must never panic; every failure path below is a log
    /// statement.
    fn on_download_event(&self, event: CoreDownloadEvent, gid: &str) {
        let Some((event_type, notification)) = Self::map_event(event, gid) else {
            return;
        };

        // `Weak::upgrade` fails once the RPC server has been dropped. Nothing
        // to publish into any more — drop the event quietly.
        let Some(engine) = self.engine.upgrade() else {
            debug!(
                event = event.name(),
                gid, "RPC engine gone; dropping download event"
            );
            return;
        };

        match engine.publisher().publish(event_type, notification) {
            Ok(receivers) => {
                debug!(
                    event = event.name(),
                    gid, receivers, "Published RPC download notification"
                );
            }
            Err(e) => {
                // The broadcast channel returns an error when there is no
                // live receiver, i.e. no WebSocket client is connected. That
                // is entirely normal, so keep it at debug level.
                debug!(
                    event = event.name(),
                    gid,
                    reason = %e,
                    "No WebSocket subscriber for download notification"
                );
            }
        }
    }

    fn is_alive(&self) -> bool {
        self.engine.strong_count() > 0
    }
}

impl App {
    /// Start the RPC HTTP server in the background with shared engine state.
    ///
    /// `group_man` and `cmd_tx` are extracted from the DownloadEngine before
    /// `run()` is called, so RPC handlers can start real downloads and query
    /// live progress.
    ///
    /// Reads RPC configuration options and creates a server instance. The
    /// caller supplies the resolved startup plan, so this method cannot
    /// accidentally restart RPC from a config-only flag during a one-shot
    /// download.
    ///
    /// Other options include:
    /// - `rpc-listen-port` — Port to listen on (default: 6800)
    /// - `rpc-listen-address` — Address to bind (default: 127.0.0.1)
    /// - `rpc-secret` — Secret token for authentication
    /// - `rpc-secure` — Enable HTTPS (requires certificate)
    /// - `rpc-certificate` — TLS certificate path
    /// - `rpc-private-key` — TLS private key path
    /// - `rpc-cors-domain` — CORS allowed origins
    ///
    /// Returns a handle to the server task on success.
    pub(super) async fn start_rpc_server<
        T: Into<aria2_core::engine::engine_command::EngineCommandSender>,
    >(
        &self,
        startup_plan: StartupPlan,
        group_man: Arc<RequestGroupMan>,
        engine_cmd_tx: T,
    ) -> std::result::Result<tokio::task::JoinHandle<()>, String> {
        if !startup_plan.starts_rpc() {
            return Err("The startup plan does not include an RPC server".to_string());
        }

        let port = self
            .get_opt_usize("rpc-listen-port")
            .await
            .unwrap_or(crate::constants::DEFAULT_RPC_PORT) as u16;
        let host = self
            .get_opt_str("rpc-listen-address")
            .await
            .unwrap_or_else(|| crate::constants::DEFAULT_RPC_HOST.to_string());
        let listen_all = self.get_opt_bool("rpc-listen-all").await.unwrap_or(false);
        let host = if listen_all {
            "0.0.0.0".to_string()
        } else {
            host
        };

        // Build authentication config
        let secret = self.get_opt_str("rpc-secret").await.unwrap_or_default();
        let user = self.get_opt_str("rpc-user").await.unwrap_or_default();
        let pass = self.get_opt_str("rpc-passwd").await;
        let auth = if !secret.is_empty() {
            AuthConfig::default().with_token(&secret)
        } else if !user.is_empty() {
            AuthConfig {
                username: Some(user),
                password: pass,
                ..AuthConfig::default()
            }
        } else {
            AuthConfig::default()
        };

        // Build CORS config
        let cors = if self
            .get_opt_bool("rpc-allow-origin-all")
            .await
            .unwrap_or(false)
        {
            CorsConfig::allow_all_origins()
        } else if let Some(allow_origin) = self
            .get_opt_str("rpc-allow-origin")
            .await
            .filter(|origin| !origin.trim().is_empty())
        {
            CorsConfig::from_option_value(&allow_origin)
        } else if let Some(cors_domain) = self
            .get_opt_str("rpc-cors-domain")
            .await
            .filter(|domain| !domain.trim().is_empty())
        {
            CorsConfig::from_option_value(&cors_domain)
        } else {
            CorsConfig::default()
        };

        // Check for TLS configuration
        let rpc_secure = self.get_opt_bool("rpc-secure").await.unwrap_or(false);
        let cert_path = self.get_opt_str("rpc-certificate").await;
        let key_path = self.get_opt_str("rpc-private-key").await;

        let save_session_path = self
            .get_opt_str("save-session")
            .await
            .map(std::path::PathBuf::from);

        // The binary owns the only adapter that knows both aria2-core and the
        // protocol-independent RPC seam. The RPC crate remains transport-only.
        let backend = Arc::new(super::rpc_backend::CoreRpcBackend::new(
            group_man,
            engine_cmd_tx.into(),
            Arc::clone(&self.config),
            save_session_path,
            crate::identity::PRODUCT_VERSION,
        ));
        let rpc_engine =
            RpcEngine::with_backend(backend).with_auth_middleware(RpcAuthMiddleware::new(&secret));

        // Build server config
        let max_request_size = self
            .get_opt_usize("rpc-max-request-size")
            .await
            .unwrap_or(aria2_rpc::constants::DEFAULT_RPC_MAX_REQUEST_SIZE);

        let mut config = ServerConfig::default()
            .with_host(&host)
            .with_port(port)
            .with_auth(auth)
            .with_cors(cors)
            .with_max_request_size(max_request_size);

        // Share the engine before the server takes ownership so the core →
        // RPC event bridge can hold a `Weak` handle to the very same
        // instance the WebSocket sessions read from.
        let rpc_engine = Arc::new(rpc_engine);

        // Install the download lifecycle bridge on the process-wide core
        // event bus. Without this, `aria2.onDownloadComplete` /
        // `aria2.onDownloadError` / `aria2.onBtDownloadComplete` are never
        // emitted, and clients such as AriaNg and webui-aria2 never see a
        // download move out of the "active" state.
        //
        // Registration must happen *before* the server starts serving so no
        // completion can slip through unobserved.
        let hooks = DownloadEventHooks::shared();
        hooks.add_listener(Arc::new(CoreEventBridge::new(&rpc_engine)));
        info!(
            listeners = hooks.listener_count(),
            "Registered core→RPC download event bridge"
        );

        // Create server with the pre-configured shared engine
        let server = if rpc_secure {
            let cert = cert_path.ok_or("rpc-certificate is required when rpc-secure is enabled")?;
            let key = key_path.ok_or("rpc-private-key is required when rpc-secure is enabled")?;
            info!("Starting HTTPS RPC server on {}:{}", host, port);
            config = config.with_tls(TlsConfig::new(cert, key));
            RpcServer::new_with_engine(config, Arc::clone(&rpc_engine))
                .map_err(|e| format!("Failed to create HTTPS RPC server: {}", e))?
        } else {
            info!("Starting HTTP RPC server on {}:{}", host, port);
            RpcServer::new_with_engine(config, Arc::clone(&rpc_engine))
                .map_err(|e| format!("Failed to create RPC server: {}", e))?
        };

        // Keep the user-requested one-shot CLI separate from RPC startup, but
        // retain aria2_original's address-family fallback when RPC is wanted.
        let disable_ipv6 = self.get_opt_bool("disable-ipv6").await.unwrap_or(false);
        let bind_hosts = rpc_bind_hosts(&host, listen_all, disable_ipv6)?;
        let mut listeners = Vec::new();
        let mut bind_errors = Vec::new();
        for bind_host in bind_hosts {
            match server.bind_listener_on(&bind_host).await {
                Ok(listener) => listeners.push(listener),
                Err(error) => bind_errors.push(format!("{bind_host}: {error}")),
            }
        }
        if listeners.is_empty() {
            return Err(format!(
                "Failed to bind RPC server on {}: {}",
                server.addr(),
                bind_errors.join("; ")
            ));
        }

        let rpc_url = server.rpc_url();
        info!("RPC server listening at {}", rpc_url);
        let output = format!("  {} RPC server: {}\n", "📡".cyan(), rpc_url.yellow());
        if self.get_opt_bool("stderr").await.unwrap_or(false) {
            eprint!("{}", output);
        } else {
            print!("{}", output);
        }

        // Spawn server in background
        let server = Arc::new(server);
        let handle = tokio::spawn(async move {
            let mut tasks = Vec::with_capacity(listeners.len());
            for listener in listeners {
                let server = Arc::clone(&server);
                tasks.push(tokio::spawn(async move {
                    if let Err(e) = server.serve_on_listener(listener).await {
                        error!("RPC server error: {}", e);
                    }
                }));
            }
            for task in tasks {
                let _ = task.await;
            }
        });

        Ok(handle)
    }
}

// Import colored for the RPC URL display
use colored::Colorize;

#[cfg(test)]
mod bridge_tests {
    use super::*;
    use tokio::sync::mpsc;

    const GID: &str = "2089b05ecca3d829";

    #[test]
    fn rpc_binding_policy_keeps_dual_stack_by_default() {
        assert_eq!(
            rpc_bind_hosts("127.0.0.1", false, false).unwrap(),
            vec!["127.0.0.1", "::1"]
        );
        assert_eq!(
            rpc_bind_hosts("0.0.0.0", true, false).unwrap(),
            vec!["0.0.0.0", "::"]
        );
    }

    #[test]
    fn rpc_binding_policy_honors_disabled_ipv6() {
        assert_eq!(
            rpc_bind_hosts("127.0.0.1", false, true).unwrap(),
            vec!["127.0.0.1"]
        );
        assert_eq!(
            rpc_bind_hosts("0.0.0.0", true, true).unwrap(),
            vec!["0.0.0.0"]
        );
        assert!(rpc_bind_hosts("::1", false, true).is_err());
    }

    /// The three terminal events must map to the C++-compatible
    /// `aria2.on*` notification methods.
    #[test]
    fn maps_terminal_events_to_rpc_notifications() {
        let cases = [
            (
                CoreDownloadEvent::Complete,
                EventType::DownloadComplete,
                "aria2.onDownloadComplete",
            ),
            (
                CoreDownloadEvent::Error,
                EventType::DownloadError,
                "aria2.onDownloadError",
            ),
            (
                CoreDownloadEvent::BtComplete,
                EventType::BtDownloadComplete,
                "aria2.onBtDownloadComplete",
            ),
        ];

        for (core_event, expected_type, expected_method) in cases {
            let (event_type, notification) = CoreEventBridge::map_event(core_event, GID)
                .unwrap_or_else(|| panic!("{:?} must be bridged", core_event));
            assert_eq!(event_type, expected_type);
            assert_eq!(notification.method(), expected_method);
            assert_eq!(notification.gid(), GID);
        }
    }

    /// Start/Pause/Stop are published by the RPC handlers themselves;
    /// bridging them here would deliver every notification twice.
    #[test]
    fn does_not_bridge_handler_published_events() {
        for core_event in [
            CoreDownloadEvent::Start,
            CoreDownloadEvent::Pause,
            CoreDownloadEvent::Stop,
        ] {
            assert!(
                CoreEventBridge::map_event(core_event, GID).is_none(),
                "{:?} must not be bridged (already published by RPC handlers)",
                core_event
            );
        }
    }

    /// End-to-end within the bridge: a core `Complete` event must land on a
    /// WebSocket subscriber as `aria2.onDownloadComplete` carrying the same
    /// 16-digit GID. This is the exact hop that was missing and that left
    /// AriaNg / webui-aria2 stuck showing finished downloads as active.
    #[tokio::test]
    async fn forwards_core_complete_to_websocket_subscriber() {
        let engine = Arc::new(RpcEngine::new());
        let mut rx = engine.publisher().subscribe("test-sub", None).await;
        let bridge = CoreEventBridge::new(&engine);

        bridge.on_download_event(CoreDownloadEvent::Complete, GID);

        let (event_type, notification) = rx.recv().await.expect("notification must be published");
        assert_eq!(event_type, EventType::DownloadComplete);
        assert_eq!(notification.method(), "aria2.onDownloadComplete");
        assert_eq!(notification.gid(), GID);

        // Start must stay silent on this path.
        bridge.on_download_event(CoreDownloadEvent::Start, GID);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "Start must not be re-published"
        );
    }

    /// The bridge holds a `Weak`, so it must degrade to a no-op (never panic,
    /// never resurrect the engine) once the RPC server has been torn down.
    #[test]
    fn tolerates_dropped_rpc_engine() {
        let engine = Arc::new(RpcEngine::new());
        let bridge = CoreEventBridge::new(&engine);
        drop(engine);

        // Must not panic — a panicking listener would unwind through the
        // download engine's state transition.
        bridge.on_download_event(CoreDownloadEvent::Complete, GID);
        assert!(
            bridge.engine.upgrade().is_none(),
            "bridge must not keep the RpcEngine alive"
        );
    }

    /// Publishing with zero WebSocket clients connected is the common case
    /// (`broadcast::Sender::send` reports "no receivers"); it must be
    /// swallowed rather than escalated.
    #[test]
    fn tolerates_absence_of_subscribers() {
        let engine = Arc::new(RpcEngine::new());
        let bridge = CoreEventBridge::new(&engine);
        bridge.on_download_event(CoreDownloadEvent::Error, GID);
    }

    #[tokio::test]
    async fn rejects_an_occupied_rpc_port_before_startup() {
        let occupied = tokio::net::TcpListener::bind("127.0.0.2:0")
            .await
            .expect("test listener should bind");
        let port = occupied
            .local_addr()
            .expect("test listener should expose an address")
            .port();

        let app = App::new();
        {
            let mut config = app.config.write().await;
            config
                .set_global_option("enable-rpc", OptionValue::Bool(true))
                .await
                .expect("enable-rpc should be valid");
            config
                .set_global_option("rpc-listen-port", OptionValue::Int(port as i64))
                .await
                .expect("rpc-listen-port should be valid");
            config
                .set_global_option(
                    "rpc-listen-address",
                    OptionValue::Str("127.0.0.2".to_string()),
                )
                .await
                .expect("rpc-listen-address should be valid");
        }

        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let error = app
            .start_rpc_server(
                StartupPlan::resolve(crate::app::startup::StartupInputs {
                    has_initial_downloads: false,
                    has_input_file: false,
                    restored_tasks: 0,
                    configured_rpc: true,
                    explicit_rpc: None,
                })
                .unwrap(),
                app.request_man.clone(),
                cmd_tx,
            )
            .await
            .expect_err("occupied port must fail RPC startup");
        assert!(error.contains("Failed to bind RPC server"));
    }

    /// Registering the bridge on the process-wide core bus must make the core
    /// side actually reach it — this is what `start_rpc_server` does.
    #[test]
    fn registers_on_the_shared_core_bus() {
        let engine = Arc::new(RpcEngine::new());
        let hooks = DownloadEventHooks::shared();
        let before = hooks.listener_count();
        hooks.add_listener(Arc::new(CoreEventBridge::new(&engine)));
        assert_eq!(hooks.listener_count(), before + 1);
    }
}
