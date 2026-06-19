//! RPC server management
//!
//! This module handles RPC server initialization and lifecycle:
//! - HTTP/HTTPS RPC server setup
//! - Authentication configuration
//! - CORS configuration

use super::App;
use aria2_rpc::server::{AuthConfig, CorsConfig, RpcServer, ServerConfig};
use tracing::{error, info};

impl App {
    /// Start the RPC HTTP server in the background.
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
    pub async fn start_rpc_server(&self) -> std::result::Result<tokio::task::JoinHandle<()>, String> {
        // Read RPC configuration
        let rpc_enabled = self.get_opt_bool("enable-rpc").await.unwrap_or(false);
        if !rpc_enabled {
            return Err("RPC is not enabled".to_string());
        }

        let port = self.get_opt_usize("rpc-listen-port").await.unwrap_or(crate::constants::DEFAULT_RPC_PORT) as u16;
        let host = self.get_opt_str("rpc-listen-address").await.unwrap_or_else(|| crate::constants::DEFAULT_RPC_HOST.to_string());

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

        // Build server config
        let config = ServerConfig::default()
            .with_host(&host)
            .with_port(port)
            .with_auth(auth)
            .with_cors(cors);

        // Create server
        let server = if rpc_secure {
            let cert = cert_path.ok_or("rpc-certificate is required when rpc-secure is enabled")?;
            let key = key_path.ok_or("rpc-private-key is required when rpc-secure is enabled")?;

            info!("Starting HTTPS RPC server on {}:{}", host, port);
            RpcServer::new_https(&host, port, &cert, &key)
                .map_err(|e| format!("Failed to create HTTPS RPC server: {}", e))?
        } else {
            info!("Starting HTTP RPC server on {}:{}", host, port);
            RpcServer::new(config)
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
