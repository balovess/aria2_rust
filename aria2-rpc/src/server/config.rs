//! Aggregated server configuration for the RPC HTTP server.

use super::auth::AuthConfig;
use super::cors::CorsConfig;
use super::tls::TlsConfig;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub auth: AuthConfig,
    pub cors: CorsConfig,
    /// TLS configuration for HTTPS RPC
    pub tls: Option<TlsConfig>,
    /// Maximum JSON-RPC/XML-RPC parser input size in bytes.
    ///
    /// HTTP rejects larger request bodies before dispatch. WebSocket keeps the
    /// connection open and maps an oversized document to aria2's JSON-RPC
    /// parse error. Default: 2 MiB.
    pub max_request_size: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 6800,
            auth: AuthConfig::default(),
            cors: CorsConfig::default(),
            tls: None,
            max_request_size: crate::constants::DEFAULT_RPC_MAX_REQUEST_SIZE,
        }
    }
}

impl ServerConfig {
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }
    pub fn with_auth(mut self, auth: AuthConfig) -> Self {
        self.auth = auth;
        self
    }
    pub fn with_cors(mut self, cors: CorsConfig) -> Self {
        self.cors = cors;
        self
    }
    pub fn with_tls(mut self, tls: TlsConfig) -> Self {
        self.tls = Some(tls);
        self
    }
    /// Set the maximum RPC parser input size in bytes.
    pub fn with_max_request_size(mut self, size: usize) -> Self {
        self.max_request_size = size;
        self
    }

    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Returns true if TLS is configured (HTTPS mode).
    pub fn is_secure(&self) -> bool {
        self.tls.is_some()
    }

    /// Returns the protocol scheme ("https" or "http").
    pub fn scheme(&self) -> &'static str {
        if self.is_secure() { "https" } else { "http" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_config_default() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.port, 6800);
        assert_eq!(cfg.addr(), "127.0.0.1:6800");
    }

    #[test]
    fn test_server_config_builder() {
        let cfg = ServerConfig::default()
            .with_port(8080)
            .with_host("0.0.0.0")
            .with_auth(AuthConfig::default().with_token("tok"));
        assert_eq!(cfg.port, 8080);
        assert!(cfg.auth.has_token());
    }

    #[test]
    fn test_server_config_with_tls() {
        let tls = TlsConfig::new("/cert.pem", "/key.pem");
        let config = ServerConfig::default().with_port(8443).with_tls(tls);

        assert!(config.is_secure());
        assert_eq!(config.scheme(), "https");
        assert!(config.tls.is_some());
    }

    #[test]
    fn test_server_config_without_tls() {
        let config = ServerConfig::default();
        assert!(!config.is_secure());
        assert_eq!(config.scheme(), "http");
        assert!(config.tls.is_none());
    }

    #[test]
    fn test_server_config_max_request_size_default() {
        let config = ServerConfig::default();
        assert_eq!(
            config.max_request_size,
            crate::constants::DEFAULT_RPC_MAX_REQUEST_SIZE
        );
        assert_eq!(config.max_request_size, 2 * 1024 * 1024);
    }

    #[test]
    fn test_server_config_with_max_request_size() {
        let config = ServerConfig::default().with_max_request_size(4 * 1024 * 1024);
        assert_eq!(config.max_request_size, 4 * 1024 * 1024);
    }
}
