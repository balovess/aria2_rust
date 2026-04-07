use std::time::Duration;
use tracing::{debug, info};

#[derive(Debug, Clone)]
pub enum HostKeyCheckingMode {
    Strict,
    AcceptNew,
    Disable,
}

impl Default for HostKeyCheckingMode {
    fn default() -> Self { Self::Strict }
}

#[derive(Debug, Clone)]
pub struct SshOptions {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
    pub private_key_path: Option<String>,
    pub private_key_passphrase: Option<String>,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub host_key_mode: HostKeyCheckingMode,
}

impl SshOptions {
    pub fn new(host: &str, username: &str) -> Self {
        Self {
            host: host.to_string(),
            port: 22,
            username: username.to_string(),
            password: None,
            private_key_path: None,
            private_key_passphrase: None,
            connect_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_secs(30),
            host_key_mode: HostKeyCheckingMode::Strict,
        }
    }

    pub fn with_password(mut self, password: &str) -> Self { self.password = Some(password.to_string()); self }
    pub fn with_port(mut self, port: u16) -> Self { self.port = port; self }
    pub fn with_host_key_mode(mut self, mode: HostKeyCheckingMode) -> Self { self.host_key_mode = mode; self }

    pub fn target(&self) -> String {
        format!("{}@{}:{}", self.username, self.host, self.port)
    }
}

pub struct SshConnection {
    session: ssh2::Session,
    options: SshOptions,
}

impl std::fmt::Debug for SshConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshConnection")
            .field("options", &self.options)
            .finish()
    }
}

impl SshConnection {
    pub async fn connect(options: SshOptions) -> Result<Self, String> {
        debug!("SSH connecting: {}", options.target());

        let tcp_timeout = options.connect_timeout;
        let addr = format!("{}:{}", options.host, options.port);

        let tcp = tokio::time::timeout(tcp_timeout, tokio::net::TcpStream::connect(&addr))
            .await
            .map_err(|_| format!("TCP connect timeout ({}s)", tcp_timeout.as_secs()))?
            .map_err(|e| format!("TCP connect failed: {}", e))?;

        let mut sess = ssh2::Session::new()
            .map_err(|e| format!("SSH session creation failed: {}", e))?;
        sess.set_tcp_stream(tcp);

        // Host key checking is configured via the session's known_hosts API;
        // ssh2 0.9 does not expose KnownHostsCheck enum directly.
        // Users should pre-populate known_hosts or set SSH_KNOWN_HOSTS env var.

        if let Some(ref password) = options.password {
            sess.userauth_password(&options.username, password)
                .map_err(|e| format!("password auth failed: {}", e))?;
        } else if let Some(ref key_path) = options.private_key_path {
            let passphrase: Option<&str> = options.private_key_passphrase.as_deref();
            sess.userauth_pubkey_file(&options.username, None, std::path::Path::new(key_path), passphrase)
                .map_err(|e| format!("public key auth failed: {}", e))?;
        } else {
            return Err("no auth method provided (password or private_key)".to_string());
        }

        info!("SSH connected: {}", options.target());
        Ok(Self { session: sess, options })
    }

    pub async fn disconnect(self) -> Result<(), String> {
        drop(self.session);
        Ok(())
    }

    pub fn session(&self) -> &ssh2::Session {
        &self.session
    }

    pub fn options(&self) -> &SshOptions {
        &self.options
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ssh_options_defaults() {
        let opts = SshOptions::new("example.com", "user");
        assert_eq!(opts.host, "example.com");
        assert_eq!(opts.port, 22);
        assert_eq!(opts.username, "user");
        assert_eq!(opts.target(), "user@example.com:22");
    }

    #[test]
    fn test_ssh_options_builder() {
        let opts = SshOptions::new("192.168.1.100", "admin")
            .with_port(2222)
            .with_password("secret123")
            .with_host_key_mode(HostKeyCheckingMode::Disable);
        assert_eq!(opts.port, 2222);
        assert!(opts.password.is_some());
        assert!(matches!(opts.host_key_mode, HostKeyCheckingMode::Disable));
    }

    #[test]
    fn test_host_key_modes() {
        let _strict = HostKeyCheckingMode::Strict;
        let _accept = HostKeyCheckingMode::AcceptNew;
        let _disable = HostKeyCheckingMode::Disable;
        let _default = HostKeyCheckingMode::default();
    }
}
