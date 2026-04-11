use std::net::SocketAddr;
use tokio::net::UdpSocket;

pub struct MockDhtNode {
    addr: SocketAddr,
}

impl MockDhtNode {
    pub async fn start(peers: Vec<SocketAddr>) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();

        let peers_clone = peers.clone();
        let socket_arc = std::sync::Arc::new(socket);

        let s = socket_arc.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match s.recv_from(&mut buf).await {
                    Ok((len, _src)) => {
                        if len > 0 {
                            let resp = Self::build_get_peers_response(&peers_clone);
                            let _ = s.send_to(&resp, _src).await;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        MockDhtNode { addr }
    }

    fn build_get_peers_response(peers: &[SocketAddr]) -> Vec<u8> {
        let mut result = format!("d4:idi20:{}{}5:valuesl", "0".repeat(20), "e").to_string();
        for p in peers {
            if let std::net::IpAddr::V4(ip4) = p.ip() {
                for b in &ip4.octets() {
                    result.push_str(&format!("{:02x}", b));
                }
                result.push_str(&format!(
                    "{:02x}{:02x}",
                    (p.port() >> 8) as u8,
                    p.port() as u8
                ));
            }
        }
        result.push_str("e");
        result.into_bytes()
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}
