use std::net::SocketAddr;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EndpointKey {
    hostname: String,
    port: u16,
}

impl EndpointKey {
    pub fn new(hostname: impl Into<String>, port: u16) -> Self {
        Self {
            hostname: hostname.into(),
            port,
        }
    }

    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionContext {
    pub endpoint: EndpointKey,
    pub peer_addr: SocketAddr,
}

impl ConnectionContext {
    pub fn new(hostname: impl Into<String>, port: u16, peer_addr: SocketAddr) -> Self {
        Self {
            endpoint: EndpointKey::new(hostname, port),
            peer_addr,
        }
    }
}
