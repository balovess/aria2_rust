use std::future::Future;
use std::time::Duration;
use tokio::net::TcpStream;

use crate::dns::dns_cache::DnsCache;
use crate::network::OutboundNetworkPolicy;
use aria2_protocol::sftp::connection::SshError;

pub(super) async fn connect_sftp_host(
    host: &str,
    port: u16,
    dns_cache: Option<&tokio::sync::Mutex<DnsCache>>,
    network_policy: &OutboundNetworkPolicy,
) -> std::result::Result<TcpStream, SshError> {
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        return network_policy
            .connect(std::net::SocketAddr::new(address, port))
            .await
            .map_err(|error| SshError::ConnectFailed {
                host: host.to_string(),
                port,
                message: error.to_string(),
            });
    }

    let Some(dns_cache) = dns_cache else {
        let addresses = crate::dns::DnsResolver::system()
            .resolve(host, port)
            .await
            .map_err(|error| SshError::ConnectFailed {
                host: host.to_string(),
                port,
                message: error.to_string(),
            })?;
        return network_policy
            .connect_candidates(&addresses)
            .await
            .map_err(|error| SshError::ConnectFailed {
                host: host.to_string(),
                port,
                message: error.to_string(),
            });
    };

    let addresses = crate::dns::DnsResolver::from_cache(dns_cache.lock().await.clone(), true)
        .resolve(host, port)
        .await
        .map_err(|error| SshError::ConnectFailed {
            host: host.to_string(),
            port,
            message: error.to_string(),
        })?;
    network_policy
        .connect_candidates(&addresses)
        .await
        .map_err(|error| SshError::ConnectFailed {
            host: host.to_string(),
            port,
            message: error.to_string(),
        })
}

pub(super) async fn connect_with_timeout<T, F>(
    host: &str,
    port: u16,
    timeout: Duration,
    connection: F,
) -> std::result::Result<T, SshError>
where
    F: Future<Output = std::result::Result<T, SshError>>,
{
    tokio::time::timeout(timeout, connection)
        .await
        .unwrap_or_else(|_| {
            Err(SshError::ConnectTimeout {
                host: host.to_string(),
                port,
                timeout_secs: timeout.as_secs(),
            })
        })
}
