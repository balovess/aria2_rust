//! Engine binding and policy adapter for the protocol-owned UDP tracker client.

#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::time::Duration;

use tracing::info;

use crate::network::OutboundNetworkPolicy;

pub use aria2_protocol::bittorrent::tracker::udp_client::{
    AnnounceResponse, ScrapeResult, UdpAnnounceParams, UdpError,
};

/// UDP tracker protocol client bound through the engine's network policy.
///
/// The protocol crate owns BEP 15 request state and wire transaction
/// management. This wrapper preserves the core constructor contract and makes
/// the engine choose the bound source address and IP address family.
pub struct UdpTrackerClient {
    inner: aria2_protocol::bittorrent::tracker::udp_client::UdpTrackerClient,
}

impl UdpTrackerClient {
    pub async fn new_with_policy(
        bind_port: u16,
        policy: &OutboundNetworkPolicy,
    ) -> Result<Self, String> {
        let socket = policy
            .bind_udp(bind_port)
            .await
            .map_err(|error| format!("UDP bind failed: {error}"))?;
        Self::from_socket(socket)
    }

    pub async fn new_with_policy_for_family(
        bind_port: u16,
        policy: &OutboundNetworkPolicy,
        ipv6: bool,
    ) -> Result<Self, String> {
        let socket = policy
            .bind_udp_for_family(bind_port, ipv6)
            .await
            .map_err(|error| format!("UDP bind failed: {error}"))?;
        Self::from_socket(socket)
    }

    fn from_socket(socket: tokio::net::UdpSocket) -> Result<Self, String> {
        let addr = socket
            .local_addr()
            .map_err(|error| format!("UDP local address unavailable: {error}"))?;
        info!(%addr, "UdpTrackerClient bound by engine policy");
        Ok(Self {
            inner: aria2_protocol::bittorrent::tracker::udp_client::UdpTrackerClient::new(socket),
        })
    }

    pub async fn announce(
        &mut self,
        params: UdpAnnounceParams<'_>,
        timeout: Duration,
    ) -> Result<AnnounceResponse, UdpError> {
        self.inner.announce(params, timeout).await
    }

    pub async fn scrape(
        &mut self,
        addr: SocketAddr,
        info_hashes: &[[u8; 20]],
        timeout: Duration,
    ) -> Result<Vec<ScrapeResult>, UdpError> {
        self.inner.scrape(addr, info_hashes, timeout).await
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

pub(crate) async fn resolve_udp_tracker_addr(
    url: &str,
    policy: &OutboundNetworkPolicy,
) -> Result<SocketAddr, String> {
    let tracker = aria2_protocol::bittorrent::tracker::udp_url::UdpTrackerUrl::parse(url).map_err(
        |error| match error {
            aria2_protocol::bittorrent::tracker::udp_url::UdpTrackerUrlError::InvalidUrl(
                message,
            ) => format!("invalid UDP tracker URL {url}: {message}"),
            aria2_protocol::bittorrent::tracker::udp_url::UdpTrackerUrlError::UnsupportedScheme => {
                format!("invalid UDP tracker URL: {url}")
            }
            aria2_protocol::bittorrent::tracker::udp_url::UdpTrackerUrlError::MissingHost => {
                format!("missing UDP tracker host: {url}")
            }
        },
    )?;

    policy
        .resolve_udp_host(tracker.host(), tracker.port())
        .await
        .map_err(|error| format!("failed to resolve UDP tracker {url}: {error}"))
}
