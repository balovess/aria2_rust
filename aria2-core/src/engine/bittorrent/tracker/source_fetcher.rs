use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use aria2_protocol::bittorrent::tracker::public_list::TrackerSourceFetcher;

use crate::dns::dns_cache::DnsCache;
use crate::http::client_identity::ClientTlsConfig;
use crate::network::OutboundNetworkPolicy;
use crate::request::request_group::DownloadOptions;

pub(crate) struct CoreTrackerSourceFetcher {
    dns_cache: Arc<tokio::sync::Mutex<DnsCache>>,
    outbound_network_policy: Arc<OutboundNetworkPolicy>,
    tls: ClientTlsConfig,
    options: DownloadOptions,
}

impl CoreTrackerSourceFetcher {
    pub(crate) fn new(
        dns_cache: Arc<tokio::sync::Mutex<DnsCache>>,
        outbound_network_policy: Arc<OutboundNetworkPolicy>,
        options: DownloadOptions,
    ) -> Self {
        Self {
            dns_cache,
            outbound_network_policy,
            tls: ClientTlsConfig::from_download_options(&options),
            options,
        }
    }

    async fn fetch_source(&self, source: &str, timeout: Duration) -> Result<String, String> {
        let url = reqwest::Url::parse(source)
            .map_err(|error| format!("Invalid tracker source URL: {error}"))?;
        let origin_host = url
            .host_str()
            .ok_or_else(|| "Tracker source URL has no host".to_string())?;
        let origin_port = url
            .port_or_known_default()
            .ok_or_else(|| "Tracker source URL has no known port".to_string())?;
        let proxy = super::http_client::configured_tracker_proxy(source, &self.options);
        let (host, port) = proxy
            .as_ref()
            .map(|(host, port, _)| (host.clone(), *port))
            .unwrap_or_else(|| (origin_host.to_string(), origin_port));
        let addresses = if let Ok(address) = host.parse::<IpAddr>() {
            vec![SocketAddr::new(address, port)]
        } else {
            self.dns_cache
                .lock()
                .await
                .resolve_with_refresh(&host, port)
                .await
                .map_err(|error| format!("Tracker source DNS lookup failed: {error}"))?
        };
        if addresses.is_empty() {
            return Err("Tracker source DNS lookup returned no addresses".to_string());
        }

        let local_address = addresses.iter().find_map(|address| {
            self.outbound_network_policy
                .source_for(*address)
                .ok()
                .flatten()
        });
        if !self.outbound_network_policy.is_direct() && local_address.is_none() {
            return Err(format!(
                "No configured outbound source can reach tracker source {host}:{port}"
            ));
        }

        let timeout_secs = timeout
            .as_secs()
            .saturating_add(u64::from(timeout.subsec_nanos() > 0))
            .max(1);
        let client = super::http_client::build_tracker_client_with_source_and_resolution(
            timeout_secs,
            timeout_secs,
            &self.tls,
            local_address,
            Some((&host, &addresses)),
            proxy.map(|(_, _, proxy)| proxy),
        )?;
        let response = client
            .get(url)
            .send()
            .await
            .map_err(|error| format!("HTTP GET failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!("HTTP error: {}", response.status()));
        }
        response
            .text()
            .await
            .map_err(|error| format!("Read body failed: {error}"))
    }
}

impl TrackerSourceFetcher for CoreTrackerSourceFetcher {
    fn fetch<'a>(
        &'a self,
        source: &'a str,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(self.fetch_source(source, timeout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tracker_source_fetch_uses_process_outbound_source_address() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("tracker-source fixture should bind");
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, remote) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            let body = b"udp://tracker.example:6969/announce\n";
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(body).await.unwrap();
            (remote.ip(),)
        });

        let source_address = "127.0.0.2".parse().unwrap();
        let fetcher = CoreTrackerSourceFetcher::new(
            Arc::new(tokio::sync::Mutex::new(DnsCache::new())),
            Arc::new(OutboundNetworkPolicy::single(source_address)),
            DownloadOptions::default(),
        );
        let body = fetcher
            .fetch(
                &format!("http://{address}/trackers.txt"),
                Duration::from_secs(2),
            )
            .await
            .expect("tracker source should be fetched");

        assert!(body.contains("tracker.example"));
        assert_eq!(server.await.unwrap().0, source_address);
    }

    #[tokio::test]
    async fn tracker_source_fetch_uses_configured_http_proxy_and_credentials() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("proxy fixture should bind");
        let proxy_address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            let size = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..size]).into_owned();
            let body = b"udp://tracker.example:6969/announce\n";
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(body).await.unwrap();
            request
        });

        let options = DownloadOptions::from_option_strings(&std::collections::HashMap::from([
            ("http-proxy".to_string(), format!("http://{proxy_address}")),
            ("http-proxy-user".to_string(), "proxy-user".to_string()),
            ("http-proxy-passwd".to_string(), "proxy-pass".to_string()),
        ]));
        let fetcher = CoreTrackerSourceFetcher::new(
            Arc::new(tokio::sync::Mutex::new(DnsCache::new())),
            Arc::new(OutboundNetworkPolicy::direct()),
            options,
        );
        fetcher
            .fetch(
                "http://tracker-source.invalid/trackers.txt",
                Duration::from_secs(2),
            )
            .await
            .expect("tracker source should be fetched through the proxy");

        let request = server.await.unwrap();
        assert!(request.starts_with("GET http://tracker-source.invalid/trackers.txt "));
        let proxy_authorization = request.lines().find_map(|line| {
            let (name, value) = line.split_once(": ")?;
            name.eq_ignore_ascii_case("Proxy-Authorization")
                .then_some(value)
        });
        assert_eq!(
            proxy_authorization,
            Some("Basic cHJveHktdXNlcjpwcm94eS1wYXNz"),
            "unexpected proxy request headers: {request}"
        );
    }
}
