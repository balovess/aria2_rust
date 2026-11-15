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
        return network_policy
            .connect_host(host, port)
            .await
            .map_err(|error| SshError::ConnectFailed {
                host: host.to_string(),
                port,
                message: error.to_string(),
            });
    };

    let dns_cache = dns_cache.lock().await.clone();
    let addresses = dns_cache
        .resolve_with_refresh(host, port)
        .await
        .map_err(|error| SshError::ConnectFailed {
            host: host.to_string(),
            port,
            message: error.to_string(),
        })?;
    let mut last_error = None;
    for address in addresses {
        match network_policy.connect(address).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }

    Err(SshError::ConnectFailed {
        host: host.to_string(),
        port,
        message: last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "DNS lookup returned no addresses".to_string()),
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
