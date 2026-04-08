use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tracing::{debug, warn};

pub struct DhtSocket {
    socket: Arc<UdpSocket>,
    local_addr: SocketAddr,
}

impl DhtSocket {
    pub async fn bind(port: u16) -> Result<Self, String> {
        let addr = format!("0.0.0.0:{}", port);
        let socket = UdpSocket::bind(&addr).await
            .map_err(|e| format!("DHT UDP bind 失败 ({}): {}", addr, e))?;
        let local_addr = socket.local_addr()
            .map_err(|e| format!("获取本地地址失败: {}", e))?;

        debug!("DHT socket 绑定到: {}", local_addr);

        Ok(Self {
            socket: Arc::new(socket),
            local_addr,
        })
    }

    pub async fn send_to(&self, addr: SocketAddr, data: &[u8]) -> Result<usize, String> {
        self.socket.send_to(data, addr).await
            .map_err(|e| format!("DHT send_to {} 失败: {}", addr, e))
    }

    pub async fn recv_with_timeout(
        &self,
        buf: &mut [u8],
        timeout: std::time::Duration,
    ) -> Result<(usize, SocketAddr), String> {
        match tokio::time::timeout(timeout, self.socket.recv_from(buf)).await {
            Ok(Ok((n, addr))) => Ok((n, addr)),
            Ok(Err(e)) => Err(format!("DHT recv 错误: {}", e)),
            Err(_) => Err("DHT recv 超时".to_string()),
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn shared_socket(&self) -> Arc<UdpSocket> {
        self.socket.clone()
    }
}

impl Clone for DhtSocket {
    fn clone(&self) -> Self {
        Self {
            socket: self.socket.clone(),
            local_addr: self.local_addr,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_bind_random_port() {
        let sock = DhtSocket::bind(0).await.expect("bind port 0 should succeed");
        let addr = sock.local_addr();
        assert!(addr.port() > 0, "should get a random port");
        assert_eq!(addr.ip(), std::net::Ipv4Addr::new(0, 0, 0, 0));
    }

    #[tokio::test]
    async fn test_bind_different_ports() {
        let sock1 = DhtSocket::bind(0).await.expect("first bind should succeed");
        let sock2 = DhtSocket::bind(0).await.expect("second bind should succeed");
        let p1 = sock1.local_addr().port();
        let p2 = sock2.local_addr().port();
        assert_ne!(p1, p2, "two random binds should get different ports");
    }

    #[tokio::test]
    async fn test_send_recv_roundtrip() {
        let sender = DhtSocket::bind(0).await.unwrap();
        let receiver = DhtSocket::bind(0).await.unwrap();
        let target_addr = std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            receiver.local_addr().port(),
        );

        let payload = b"hello dht";
        sender.send_to(target_addr, payload).await.unwrap();

        let mut buf = [0u8; 64];
        let (n, from) = receiver.recv_with_timeout(&mut buf, std::time::Duration::from_secs(2)).await
            .expect("should receive message");

        assert_eq!(n, payload.len());
        assert_eq!(&buf[..n], payload);
        assert_eq!(from.ip(), std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    }

    #[tokio::test]
    async fn test_recv_timeout() {
        let sock = DhtSocket::bind(0).await.unwrap();
        let mut buf = [0u8; 64];
        let result = sock.recv_with_timeout(&mut buf, std::time::Duration::from_millis(10)).await;

        assert!(result.is_err(), "recv with short timeout should fail");
        assert!(result.unwrap_err().contains("超时"));
    }

    #[tokio::test]
    async fn test_shared_socket_same_instance() {
        let sock = DhtSocket::bind(0).await.unwrap();
        let s1 = sock.shared_socket();
        let s2 = sock.shared_socket();
        assert!(Arc::ptr_eq(&s1, &s2), "shared_socket should return same Arc");
    }

    #[tokio::test]
    async fn test_clone_preserves_address() {
        let sock = DhtSocket::bind(0).await.unwrap();
        let addr1 = sock.local_addr();
        let cloned = sock.clone();
        let addr2 = cloned.local_addr();
        assert_eq!(addr1, addr2, "clone should preserve local address");
    }
}
