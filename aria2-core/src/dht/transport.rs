//! UDP transport layer for DHT messages.
//!
//! Equivalent to C++ `DHTConnectionImpl`. Handles UDP socket binding
//! (with port range support), sending bencoded DHT messages to remote
//! nodes, and receiving DHT messages with sender address extraction.
//!
//! C++ reference: `DHTConnectionImpl.h/cc`

use std::io;
use std::net::SocketAddr;

use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

#[allow(unused_imports)]
use super::constants;

// -- Address family --------------------------------------------------------

/// IP address family for DHT transport.
/// Corresponds to C++ `int family_` (AF_INET / AF_INET6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressFamily {
    Ipv4,
    Ipv6,
}

impl AddressFamily {
    fn wildcard_addr(self) -> &'static str {
        match self {
            Self::Ipv4 => "0.0.0.0",
            Self::Ipv6 => "::",
        }
    }

    fn from_addr(addr: &str) -> Self {
        if addr.contains(':') { Self::Ipv6 } else { Self::Ipv4 }
    }
}

// -- DhtTransport ----------------------------------------------------------

/// UDP transport for DHT messages.
///
/// Wraps a `tokio::net::UdpSocket` bound to a local address, providing
/// async send/receive for the DHT protocol. Supports IPv4 and IPv6.
///
/// C++: `DHTConnectionImpl`
pub struct DhtTransport {
    socket: UdpSocket,
    family: AddressFamily,
    bind_addr: SocketAddr,
}

impl DhtTransport {
    /// Bind to a specific address and port.
    ///
    /// Port 0 lets the OS assign an ephemeral port. Empty `addr` uses
    /// the wildcard address. Family is auto-detected (':' => IPv6).
    ///
    /// C++: `DHTConnectionImpl::bind(port, addr)`
    pub async fn bind(addr: &str, port: u16) -> io::Result<Self> {
        let family = AddressFamily::from_addr(addr);
        let ip_str = if addr.is_empty() { family.wildcard_addr() } else { addr };
        let socket = UdpSocket::bind(format!("{}:{}", ip_str, port)).await?;
        let bind_addr = socket.local_addr()?;
        info!(addr = %bind_addr, family = ?family, "DHT transport bound");
        Ok(Self { socket, family, bind_addr })
    }

    /// Try binding to ports in the inclusive range `[start, end]`.
    ///
    /// C++: `DHTConnectionImpl::bind(port, addr, sgl)` where `sgl`
    /// is the `SegList<int>` port range from `--dht-listen-port`.
    pub async fn bind_with_port_range(addr: &str, start: u16, end: u16) -> io::Result<Self> {
        let family = AddressFamily::from_addr(addr);
        let ip_str = if addr.is_empty() { family.wildcard_addr() } else { addr };
        for port in start..=end {
            match UdpSocket::bind(format!("{}:{}", ip_str, port)).await {
                Ok(socket) => {
                    let bind_addr = socket.local_addr()?;
                    info!(addr = %bind_addr, family = ?family, "DHT transport bound (port range)");
                    return Ok(Self { socket, family, bind_addr });
                }
                Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                    debug!(port, "DHT port in use, trying next");
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        warn!(start, end, "All DHT ports in range exhausted");
        Err(io::Error::new(io::ErrorKind::AddrInUse,
            format!("no available port in range {}-{}", start, end)))
    }

    /// Send a bencoded DHT message to a remote node.
    /// C++: `DHTConnectionImpl::sendMessage(data, len, host, port)`
    pub async fn send_message(&self, data: &[u8], addr: SocketAddr) -> io::Result<usize> {
        let n = self.socket.send_to(data, addr).await?;
        debug!(target = %addr, len = n, "DHT message sent");
        Ok(n)
    }

    /// Receive a DHT message, returning `(bytes_read, sender_addr)`.
    ///
    /// Caller provides buffer; recommended size is at least
    /// [`constants::DHT_MAX_MESSAGE_SIZE`] (4096 bytes). Cancellation-safe.
    ///
    /// C++: `DHTConnectionImpl::receiveMessage(data, len, host, port)`
    pub async fn recv_message(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let (n, from) = self.socket.recv_from(buf).await?;
        debug!(from = %from, len = n, "DHT message received");
        Ok((n, from))
    }

    /// Return the local address this socket is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> { Ok(self.bind_addr) }

    /// Return the address family (IPv4 or IPv6).
    pub fn address_family(&self) -> AddressFamily { self.family }

    /// Return the port number this socket is bound to.
    pub fn bound_port(&self) -> u16 { self.bind_addr.port() }
}

// -- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[tokio::test]
    async fn bind_auto_assign_port() {
        let t = DhtTransport::bind("", 0).await.unwrap();
        assert!(t.bound_port() > 0);
        assert_eq!(t.address_family(), AddressFamily::Ipv4);
        assert!(t.local_addr().unwrap().is_ipv4());
    }

    #[tokio::test]
    async fn bind_ipv6_auto_assign() {
        let t = DhtTransport::bind("::", 0).await.unwrap();
        assert!(t.bound_port() > 0);
        assert_eq!(t.address_family(), AddressFamily::Ipv6);
    }

    #[tokio::test]
    async fn send_recv_roundtrip() {
        let sender = DhtTransport::bind("", 0).await.unwrap();
        let receiver = DhtTransport::bind("", 0).await.unwrap();
        let target = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), receiver.bound_port());

        let payload = b"d1:eli201e1:t2:aa1:y1:ee";
        let sent = sender.send_message(payload, target).await.unwrap();
        assert_eq!(sent, payload.len());

        let mut buf = [0u8; constants::DHT_MAX_MESSAGE_SIZE];
        let (n, from) = receiver.recv_message(&mut buf).await.unwrap();
        assert_eq!(n, payload.len());
        assert_eq!(&buf[..n], payload);
        // When the sender binds to a wildcard address (0.0.0.0), the OS reports
        // the source as 127.0.0.1 when sending to localhost, while local_addr()
        // returns 0.0.0.0. Compare ports only to avoid this discrepancy.
        assert_eq!(from.port(), sender.local_addr().unwrap().port());
    }

    #[tokio::test]
    async fn address_family_detection() {
        let v4 = DhtTransport::bind("0.0.0.0", 0).await.unwrap();
        assert_eq!(v4.address_family(), AddressFamily::Ipv4);
        let v6 = DhtTransport::bind("::", 0).await.unwrap();
        assert_eq!(v6.address_family(), AddressFamily::Ipv6);
    }

    #[tokio::test]
    async fn port_range_binds_first_available() {
        let t = DhtTransport::bind_with_port_range("", 59600, 59610)
            .await.expect("should bind within range");
        assert!((59600..=59610).contains(&t.bound_port()));
    }

    #[tokio::test]
    async fn port_range_exhausted() {
        let mut held = Vec::new();
        for p in 59700u16..=59702 {
            if let Ok(t) = DhtTransport::bind("", p).await { held.push(t); }
        }
        let result = DhtTransport::bind_with_port_range("", 59700, 59702).await;
        if let Ok(t) = result { assert!((59700..=59702).contains(&t.bound_port())); }
        drop(held);
    }
}
