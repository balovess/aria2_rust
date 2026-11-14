use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aria2_protocol::ftp::connection::FtpControlSession;
use aria2_protocol::ftp::tls::{FtpControlStream, FtpsClient, FtpsConfig, TlsVersion};

use crate::ftp::connection::FtpProxyConfig;
use crate::network::{ConnectionContext, OutboundNetworkPolicy};

/// Identity of a control connection's proxy route.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ProxySessionKey {
    host: String,
    port: u16,
    username: String,
    password: String,
}

/// TLS policy is part of the connection identity so sessions cannot cross
/// certificate-validation or implicit/explicit FTPS configurations.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TlsSessionKey {
    enabled: bool,
    check_certificate: bool,
    ca_certificate: Option<PathBuf>,
    min_tls_version: TlsVersion,
    implicit: bool,
}

/// The engine-level identity used to lease an already-authenticated FTP
/// control session. Per-transfer choices such as passive mode and TYPE are
/// deliberately omitted because each download configures those after lease.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct ControlSessionKey {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) username: String,
    password: String,
    proxy: Option<ProxySessionKey>,
    tls: Option<TlsSessionKey>,
    outbound_sources: Vec<IpAddr>,
}

impl std::fmt::Debug for ControlSessionKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControlSessionKey")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field(
                "proxy",
                &self.proxy.as_ref().map(|proxy| (&proxy.host, proxy.port)),
            )
            .field("tls", &self.tls)
            .field("outbound_sources", &self.outbound_sources)
            .finish()
    }
}

impl ControlSessionKey {
    pub(crate) fn new(
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        proxy: Option<&FtpProxyConfig>,
        tls: Option<&FtpsConfig>,
        implicit_tls: bool,
        outbound_sources: Vec<IpAddr>,
    ) -> Self {
        Self {
            host: host.to_owned(),
            port,
            username: username.to_owned(),
            password: password.to_owned(),
            proxy: proxy.map(|proxy| ProxySessionKey {
                host: proxy.proxy_host.clone(),
                port: proxy.proxy_port,
                username: proxy.proxy_username.clone(),
                password: proxy.proxy_password.clone(),
            }),
            tls: tls.map(|tls| TlsSessionKey {
                enabled: tls.enabled,
                check_certificate: tls.check_certificate,
                ca_certificate: tls.ca_certificate.clone(),
                min_tls_version: tls.min_tls_version,
                implicit: implicit_tls,
            }),
            outbound_sources,
        }
    }
}

/// An authenticated FTP control session leased between download commands.
pub(crate) struct PooledControlSession {
    pub(crate) session: FtpControlSession<FtpControlStream>,
    pub(crate) connection: ConnectionContext,
    pub(crate) ftps_client: Option<FtpsClient>,
    pub(crate) outbound_network_policy: Arc<OutboundNetworkPolicy>,
    created_at: Instant,
    last_used: Instant,
    reuse_count: u64,
}

impl PooledControlSession {
    pub(crate) fn new(
        session: FtpControlSession<FtpControlStream>,
        connection: ConnectionContext,
        ftps_client: Option<FtpsClient>,
        outbound_network_policy: Arc<OutboundNetworkPolicy>,
    ) -> Self {
        let now = Instant::now();
        Self {
            session,
            connection,
            ftps_client,
            outbound_network_policy,
            created_at: now,
            last_used: now,
            reuse_count: 0,
        }
    }

    pub(crate) fn mark_used(&mut self) {
        self.last_used = Instant::now();
        self.reuse_count += 1;
    }

    pub(crate) fn reuse_count(&self) -> u64 {
        self.reuse_count
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        FtpControlSession<FtpControlStream>,
        ConnectionContext,
        Option<FtpsClient>,
        Arc<OutboundNetworkPolicy>,
        Instant,
        Instant,
        u64,
    ) {
        (
            self.session,
            self.connection,
            self.ftps_client,
            self.outbound_network_policy,
            self.created_at,
            self.last_used,
            self.reuse_count,
        )
    }

    pub(crate) fn restore_lifetime(&mut self, created_at: Instant, reuse_count: u64) {
        self.created_at = created_at;
        self.last_used = Instant::now();
        self.reuse_count = reuse_count;
    }

    pub(crate) fn is_reusable(
        &self,
        max_idle_time: Duration,
        max_connection_age: Duration,
    ) -> bool {
        self.last_used.elapsed() < max_idle_time && self.created_at.elapsed() <= max_connection_age
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_session_identity_separates_credentials_and_redacts_them() {
        let first = ControlSessionKey::new(
            "ftp.example.test",
            21,
            "download-user",
            "origin-secret",
            None,
            None,
            false,
            Vec::new(),
        );
        let different_password = ControlSessionKey::new(
            "ftp.example.test",
            21,
            "download-user",
            "different-secret",
            None,
            None,
            false,
            Vec::new(),
        );
        assert_ne!(first, different_password);
        assert!(!format!("{first:?}").contains("origin-secret"));
        assert!(!format!("{different_password:?}").contains("different-secret"));
    }
}
