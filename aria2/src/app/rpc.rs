//! RPC server management
//!
//! This module handles RPC server initialization and lifecycle:
//! - HTTP/HTTPS RPC server setup
//! - Authentication configuration
//! - CORS configuration
//! - Shared engine state (RequestGroupMan + command channel)

use super::App;
use aria2_core::config::OptionValue;
use aria2_core::engine::command::Command;
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_rpc::engine::RpcEngine;
use aria2_rpc::server::{
    AuthConfig, CorsConfig, RpcAuthMiddleware, RpcServer, ServerConfig, TlsConfig,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use tracing::{error, info};

impl App {
    /// Start the RPC HTTP server in the background with shared engine state.
    ///
    /// `group_man` and `cmd_tx` are extracted from the DownloadEngine before
    /// `run()` is called, so RPC handlers can start real downloads and query
    /// live progress.
    ///
    /// Reads RPC configuration options and creates a server instance:
    /// - `enable-rpc` — Enable/disable RPC server
    /// - `rpc-listen-port` — Port to listen on (default: 6800)
    /// - `rpc-listen-address` — Address to bind (default: 127.0.0.1)
    /// - `rpc-secret` — Secret token for authentication
    /// - `rpc-secure` — Enable HTTPS (requires certificate)
    /// - `rpc-certificate` — TLS certificate path
    /// - `rpc-private-key` — TLS private key path
    /// - `rpc-cors-domain` — CORS allowed origins
    ///
    /// Returns a handle to the server task on success.
    pub async fn start_rpc_server(
        &self,
        group_man: Arc<RwLock<RequestGroupMan>>,
        cmd_tx: mpsc::UnboundedSender<Box<dyn Command>>,
    ) -> std::result::Result<tokio::task::JoinHandle<()>, String> {
        // Read RPC configuration
        let rpc_enabled = self.get_opt_bool("enable-rpc").await.unwrap_or(false);
        if !rpc_enabled {
            return Err("RPC is not enabled".to_string());
        }

        let port = self
            .get_opt_usize("rpc-listen-port")
            .await
            .unwrap_or(crate::constants::DEFAULT_RPC_PORT) as u16;
        let host = self
            .get_opt_str("rpc-listen-address")
            .await
            .unwrap_or_else(|| crate::constants::DEFAULT_RPC_HOST.to_string());

        // Build authentication config
        let auth = if let Some(secret) = self.get_opt_str("rpc-secret").await {
            AuthConfig::default().with_token(&secret)
        } else if let (Some(user), Some(pass)) = (
            self.get_opt_str("rpc-user").await,
            self.get_opt_str("rpc-passwd").await,
        ) {
            AuthConfig::default().with_basic_auth(&user, &pass)
        } else {
            AuthConfig::default()
        };

        // Build CORS config
        let cors = if let Some(cors_domain) = self.get_opt_str("rpc-cors-domain").await {
            CorsConfig::from_option_value(&cors_domain)
        } else {
            CorsConfig::default()
        };

        // Check for TLS configuration
        let rpc_secure = self.get_opt_bool("rpc-secure").await.unwrap_or(false);
        let cert_path = self.get_opt_str("rpc-certificate").await;
        let key_path = self.get_opt_str("rpc-private-key").await;

        // Build RPC engine with shared state (group_man + cmd_tx) so that
        // aria2.addUri starts real downloads and tellStatus/getGlobalStat
        // read live progress.
        let secret = self.get_opt_str("rpc-secret").await.unwrap_or_default();

        // Collect user-set global options from ConfigManager and merge them
        // over the OptionRegistry defaults inside the RPC engine. User values
        // take precedence; null values fall back to defaults.
        let config_snapshot = self.config.read().await;
        let user_opts = config_snapshot.get_all_global_options().await;
        drop(config_snapshot);

        // Convert HashMap<String, OptionValue> to HashMap<String, serde_json::Value>.
        // Only `From<&OptionValue>` is implemented, so borrow each value during
        // conversion instead of consuming it.
        let user_opts_json: HashMap<String, serde_json::Value> = user_opts
            .into_iter()
            .map(|(k, v)| (k, <&OptionValue as Into<serde_json::Value>>::into(&v)))
            .collect();

        let rpc_engine = RpcEngine::new()
            .with_auth_middleware(RpcAuthMiddleware::new(&secret))
            .with_group_man(group_man)
            .with_cmd_tx(cmd_tx)
            .with_global_opts(user_opts_json);

        // Pass the configured --save-session path through so the RPC
        // `aria2.saveSession` method can persist without an explicit path
        // argument (mirrors C++ reading PREF_SAVE_SESSION).
        let rpc_engine = if let Some(save_path) =
            self.get_opt_str("save-session").await.map(std::path::PathBuf::from)
        {
            rpc_engine.with_save_session_path(save_path)
        } else {
            rpc_engine
        };

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

        // Create server with the pre-configured shared engine
        let server = if rpc_secure {
            let cert = cert_path.ok_or("rpc-certificate is required when rpc-secure is enabled")?;
            let key = key_path.ok_or("rpc-private-key is required when rpc-secure is enabled")?;
            info!("Starting HTTPS RPC server on {}:{}", host, port);
            config = config.with_tls(TlsConfig::new(cert, key));
            RpcServer::new_with_engine(config, Arc::new(rpc_engine))
                .map_err(|e| format!("Failed to create HTTPS RPC server: {}", e))?
        } else {
            info!("Starting HTTP RPC server on {}:{}", host, port);
            RpcServer::new_with_engine(config, Arc::new(rpc_engine))
                .map_err(|e| format!("Failed to create RPC server: {}", e))?
        };

        let rpc_url = server.rpc_url();
        info!("RPC server listening at {}", rpc_url);
        println!("  {} RPC server: {}", "📡".cyan(), rpc_url.yellow());

        // Spawn server in background
        let handle = tokio::spawn(async move {
            if let Err(e) = server.serve().await {
                error!("RPC server error: {}", e);
            }
        });

        Ok(handle)
    }
}

// Import colored for the RPC URL display
use colored::Colorize;
