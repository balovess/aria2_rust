use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use tokio::net::TcpStream;

use super::connect_from;

/// The process-wide policy used by every outbound TCP adapter.
///
/// A policy owns an immutable set of source addresses and only keeps small
/// per-source counters on the hot path.  New connections choose the least
/// loaded healthy source for their address family; established connections are
/// still reused by the protocol-specific pools above this seam.
#[derive(Clone, Debug)]
pub struct OutboundNetworkPolicy {
    sources: Arc<[SourceState]>,
}

#[derive(Debug)]
struct SourceState {
    address: IpAddr,
    in_flight: AtomicUsize,
    failures: AtomicU32,
}

impl SourceState {
    fn new(address: IpAddr) -> Self {
        Self {
            address,
            in_flight: AtomicUsize::new(0),
            failures: AtomicU32::new(0),
        }
    }

    fn score(&self) -> usize {
        self.in_flight
            .load(Ordering::Relaxed)
            .saturating_mul(16)
            .saturating_add(self.failures.load(Ordering::Relaxed) as usize)
    }
}

impl OutboundNetworkPolicy {
    /// Create the direct-routing policy used when `--interface` is absent.
    pub fn direct() -> Self {
        Self {
            sources: Arc::new([]),
        }
    }

    pub fn new(addresses: Vec<IpAddr>) -> Result<Self, String> {
        let mut unique = Vec::with_capacity(addresses.len());
        for address in addresses {
            if !unique.contains(&address) {
                unique.push(address);
            }
        }
        if unique.is_empty() {
            return Err("at least one outbound source address is required".to_string());
        }
        Ok(Self {
            sources: unique
                .into_iter()
                .map(SourceState::new)
                .collect::<Vec<_>>()
                .into(),
        })
    }

    pub fn single(address: IpAddr) -> Self {
        Self::new(vec![address]).expect("a single source address is never empty")
    }

    pub fn is_direct(&self) -> bool {
        self.sources.is_empty()
    }

    pub fn addresses(&self) -> Vec<IpAddr> {
        self.sources.iter().map(|source| source.address).collect()
    }

    /// Resolve an aria2-style interface specification into source addresses.
    ///
    /// IP literals are retained as-is.  Host names are resolved through Tokio's
    /// resolver, preserving the existing CLI behaviour for host-based
    /// interface values while keeping resolution out of connection hot paths.
    pub async fn resolve_spec(spec: &str) -> Result<Self, String> {
        let mut addresses = Vec::new();
        for value in spec
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if let Ok(address) = value.parse::<IpAddr>() {
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
                continue;
            }

            let interface_addresses = interface_addresses(value);
            if !interface_addresses.is_empty() {
                for address in interface_addresses {
                    if !addresses.contains(&address) {
                        addresses.push(address);
                    }
                }
                continue;
            }

            let resolved = tokio::net::lookup_host((value, 0))
                .await
                .map_err(|error| format!("cannot resolve '{value}': {error}"))?;
            for address in resolved.map(|address| address.ip()) {
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
        }
        Self::new(addresses)
    }

    /// Select a source for a protocol client that owns its own connection
    /// pool, such as reqwest.  `None` means the OS should choose the source.
    pub fn source_for(&self, remote: SocketAddr) -> io::Result<Option<IpAddr>> {
        let Some(source) = self.best_source(remote) else {
            if self.is_direct() {
                return Ok(None);
            }
            return Err(family_mismatch(remote));
        };
        Ok(Some(source.address))
    }

    /// Resolve a host and select a compatible local source for a client that
    /// owns its connection pool (for example reqwest).
    pub async fn source_for_host(&self, host: &str, port: u16) -> io::Result<Option<IpAddr>> {
        let addresses = tokio::net::lookup_host((host, port)).await?;
        for remote in addresses {
            if let Ok(source) = self.source_for(remote) {
                return Ok(source);
            }
        }
        Err(family_mismatch(SocketAddr::new(
            IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            port,
        )))
    }

    /// Establish a TCP connection to a hostname through the policy.
    pub async fn connect_host(&self, host: &str, port: u16) -> io::Result<TcpStream> {
        let addresses = tokio::net::lookup_host((host, port)).await?;
        let mut last_error = None;
        for remote in addresses {
            if !self.is_direct() && self.best_source(remote).is_none() {
                continue;
            }
            match self.connect(remote).await {
                Ok(stream) => return Ok(stream),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::AddrNotAvailable, "no compatible address")
        }))
    }

    /// Establish a TCP connection through the policy.  This is the common
    /// seam used by FTP, SFTP and BitTorrent adapters.
    pub async fn connect(&self, remote: SocketAddr) -> io::Result<TcpStream> {
        let Some(source) = self.best_source(remote) else {
            if self.is_direct() {
                return TcpStream::connect(remote).await;
            }
            return Err(family_mismatch(remote));
        };

        source.in_flight.fetch_add(1, Ordering::Relaxed);
        let result = connect_from(source.address, remote).await;
        source.in_flight.fetch_sub(1, Ordering::Relaxed);
        if result.is_err() {
            source
                .failures
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    Some(value.saturating_add(1))
                })
                .ok();
        } else {
            source
                .failures
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    Some(value.saturating_sub(1))
                })
                .ok();
        }
        result
    }

    /// Bind an outbound UDP socket to a configured source address. UDP
    /// clients keep this socket and reuse it for all datagrams.
    pub async fn bind_udp(&self, port: u16) -> io::Result<tokio::net::UdpSocket> {
        let local = self
            .sources
            .first()
            .map(|source| SocketAddr::new(source.address, port))
            .unwrap_or_else(|| SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), port));
        tokio::net::UdpSocket::bind(local).await
    }

    /// Bind an outbound UDP socket for a specific address family.
    ///
    /// UDP sockets are family-specific, so callers that talk to both IPv4 and
    /// IPv6 endpoints must create one socket per family instead of relying on
    /// the first configured source address.
    pub async fn bind_udp_for_family(
        &self,
        port: u16,
        ipv6: bool,
    ) -> io::Result<tokio::net::UdpSocket> {
        let local = if self.is_direct() {
            SocketAddr::new(
                if ipv6 {
                    IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
                } else {
                    IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
                },
                port,
            )
        } else {
            let source = self
                .sources
                .iter()
                .filter(|source| source.address.is_ipv6() == ipv6)
                .min_by_key(|source| source.score())
                .ok_or_else(|| {
                    family_mismatch(SocketAddr::new(
                        if ipv6 {
                            IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
                        } else {
                            IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
                        },
                        port,
                    ))
                })?;
            SocketAddr::new(source.address, port)
        };
        tokio::net::UdpSocket::bind(local).await
    }

    fn best_source(&self, remote: SocketAddr) -> Option<&SourceState> {
        self.sources
            .iter()
            .filter(|source| source.address.is_ipv4() == remote.is_ipv4())
            .min_by_key(|source| source.score())
    }
}

#[cfg(unix)]
fn interface_addresses(name: &str) -> Vec<IpAddr> {
    use std::ffi::CStr;
    use std::ptr;

    let mut result = Vec::new();
    unsafe {
        let mut head = ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return result;
        }
        let mut current = head;
        while !current.is_null() {
            let interface = &*current;
            let matches = !interface.ifa_name.is_null()
                && CStr::from_ptr(interface.ifa_name).to_bytes() == name.as_bytes();
            if matches && !interface.ifa_addr.is_null() {
                match (*interface.ifa_addr).sa_family as i32 {
                    libc::AF_INET => {
                        let address = *(interface.ifa_addr as *const libc::sockaddr_in);
                        result.push(IpAddr::V4(std::net::Ipv4Addr::from(
                            u32::from_be(address.sin_addr.s_addr).to_be_bytes(),
                        )));
                    }
                    libc::AF_INET6 => {
                        let address = *(interface.ifa_addr as *const libc::sockaddr_in6);
                        result.push(IpAddr::V6(std::net::Ipv6Addr::from(
                            address.sin6_addr.s6_addr,
                        )));
                    }
                    _ => {}
                }
            }
            current = interface.ifa_next;
        }
        libc::freeifaddrs(head);
    }
    result
}

#[cfg(windows)]
fn interface_addresses(name: &str) -> Vec<IpAddr> {
    use std::ffi::CStr;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6,
    };

    let mut size = 15_000u32;
    let mut buffer = vec![0u8; size as usize];
    let status = unsafe {
        GetAdaptersAddresses(
            AF_UNSPEC as u32,
            0,
            std::ptr::null(),
            buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
            &mut size,
        )
    };
    if status != 0 {
        return Vec::new();
    }

    let mut result = Vec::new();
    let mut adapter = buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
    while !adapter.is_null() {
        let current = unsafe { &*adapter };
        let adapter_name = if current.AdapterName.is_null() {
            "".to_string()
        } else {
            unsafe { CStr::from_ptr(current.AdapterName as *const i8) }
                .to_string_lossy()
                .into_owned()
        };
        let friendly_name = if current.FriendlyName.is_null() {
            String::new()
        } else {
            let mut length = 0;
            unsafe {
                while *current.FriendlyName.add(length) != 0 {
                    length += 1;
                }
                String::from_utf16_lossy(std::slice::from_raw_parts(current.FriendlyName, length))
            }
        };
        if adapter_name == name || friendly_name == name {
            let mut address = current.FirstUnicastAddress;
            while !address.is_null() {
                let socket_address = unsafe { (*address).Address.lpSockaddr };
                if !socket_address.is_null() {
                    let family = unsafe { (*socket_address).sa_family };
                    unsafe {
                        if family == AF_INET {
                            let value = *(socket_address as *const SOCKADDR_IN);
                            result.push(IpAddr::V4(std::net::Ipv4Addr::from(
                                value.sin_addr.S_un.S_addr.to_ne_bytes(),
                            )));
                        } else if family == AF_INET6 {
                            let value = *(socket_address as *const SOCKADDR_IN6);
                            result
                                .push(IpAddr::V6(std::net::Ipv6Addr::from(value.sin6_addr.u.Byte)));
                        }
                    }
                }
                address = unsafe { (*address).Next };
            }
            break;
        }
        adapter = current.Next;
    }
    result
}

fn family_mismatch(remote: SocketAddr) -> io::Error {
    io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        format!("no configured outbound source matches {remote}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn policy_connect_uses_configured_source_address() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let remote = listener.local_addr().unwrap();
        let policy = OutboundNetworkPolicy::single(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));

        let client = policy.connect(remote).await.unwrap();
        let (_, peer) = listener.accept().await.unwrap();
        assert_eq!(client.local_addr().unwrap().ip(), peer.ip());
        assert_eq!(peer.ip(), IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    }

    #[tokio::test]
    async fn policy_rejects_a_family_without_a_matching_source() {
        let policy = OutboundNetworkPolicy::single(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        let error = policy
            .connect(SocketAddr::new(std::net::Ipv6Addr::LOCALHOST.into(), 1))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrNotAvailable);
    }

    #[test]
    fn source_selection_prefers_the_less_loaded_source() {
        let policy = OutboundNetworkPolicy::new(vec![
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 2)),
        ])
        .unwrap();
        policy.sources[0].in_flight.store(3, Ordering::Relaxed);
        assert_eq!(
            policy.source_for("127.0.0.1:1".parse().unwrap()).unwrap(),
            Some(IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 2)))
        );
    }
}
