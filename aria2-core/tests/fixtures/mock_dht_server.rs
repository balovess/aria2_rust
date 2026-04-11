use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio::sync::{Mutex, Notify};

use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use aria2_protocol::bittorrent::dht::message::{
    DhtMessage, DhtMessageBuilder, DhtMessageType, DhtQueryMethod,
};

struct DhtExpectation {
    response: Option<DhtMessage>,
    peers: Vec<SocketAddr>,
    nodes: Vec<(SocketAddr, [u8; 20])>,
}

pub struct MockDhtServer {
    socket: Arc<UdpSocket>,
    addr: SocketAddr,
    shutdown: Arc<Notify>,
    expectations: Arc<Mutex<BTreeMap<String, DhtExpectation>>>,
    received_messages: Arc<Mutex<Vec<DhtMessage>>>,
}

impl MockDhtServer {
    pub async fn bind(port: u16) -> Result<Self, String> {
        let socket = UdpSocket::bind(format!("127.0.0.1:{}", port))
            .await
            .map_err(|e| format!("MockDhtServer bind failed: {}", e))?;
        let addr = socket
            .local_addr()
            .map_err(|e| format!("get local_addr failed: {}", e))?;

        let shutdown = Arc::new(Notify::new());
        let expectations = Arc::new(Mutex::new(BTreeMap::new()));
        let received = Arc::new(Mutex::new(Vec::new()));

        let server = Self {
            socket: Arc::new(socket),
            addr,
            shutdown: shutdown.clone(),
            expectations: expectations.clone(),
            received_messages: received.clone(),
        };

        let sock = server.socket.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            loop {
                tokio::select! {
                    result = sock.recv_from(&mut buf) => {
                        match result {
                            Ok((len, src)) => {
                                if len == 0 { continue; }
                                match DhtMessage::decode(&buf[..len]) {
                                    Ok(msg) => {
                                        let mut recv = received.lock().await;
                                        recv.push(msg.clone());
                                        drop(recv);

                                        let resp = Self::build_response(&msg, &expectations).await;
                                        if let Some(data) = resp {
                                            let _ = sock.send_to(&data, src).await;
                                        }
                                    }
                                    Err(_) => {}
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    _ = shutdown.notified() => break,
                }
            }
        });

        Ok(server)
    }

    async fn build_response(
        msg: &DhtMessage,
        expectations: &Arc<Mutex<BTreeMap<String, DhtExpectation>>>,
    ) -> Option<Vec<u8>> {
        match msg.y {
            DhtMessageType::Query => {
                let method = msg.q.as_ref()?.0.as_str();
                let exp_map = expectations.lock().await;

                if method == DhtQueryMethod::PING {
                    if let Some(exp) = exp_map.get("ping") {
                        if let Some(ref resp) = exp.response {
                            return resp.encode().ok();
                        }
                        return Some(Self::pong_response(&msg.t).encode().ok()?);
                    }
                }

                if method == DhtQueryMethod::GET_PEERS {
                    if let Some(exp) = exp_map.get("get_peers") {
                        return Some(
                            Self::get_peers_response(&msg.t, &exp.peers, &exp.nodes)
                                .encode()
                                .ok()?,
                        );
                    }
                }

                if method == DhtQueryMethod::FIND_NODE {
                    if let Some(exp) = exp_map.get("find_node") {
                        return Some(Self::find_node_response(&msg.t, &exp.nodes).encode().ok()?);
                    }
                }

                if method == DhtQueryMethod::ANNOUNCE_PEER {
                    drop(exp_map);
                    return None;
                }

                None
            }
            _ => None,
        }
    }

    fn pong_response(tx_id: &[u8]) -> DhtMessage {
        let mut r_dict = BTreeMap::new();
        r_dict.insert(b"id".to_vec(), BencodeValue::Bytes(vec![0x42u8; 20]));
        DhtMessage::new_response(tx_id.to_vec(), BencodeValue::Dict(r_dict))
    }

    fn get_peers_response(
        tx_id: &[u8],
        peers: &[SocketAddr],
        nodes: &[(SocketAddr, [u8; 20])],
    ) -> DhtMessage {
        let mut r_dict = BTreeMap::new();
        r_dict.insert(b"id".to_vec(), BencodeValue::Bytes(vec![0x42u8; 20]));

        if !peers.is_empty() {
            let peer_values: Vec<BencodeValue> = peers
                .iter()
                .map(|p| match p.ip() {
                    std::net::IpAddr::V4(ip4) => BencodeValue::Bytes(vec![
                        ip4.octets()[0],
                        ip4.octets()[1],
                        ip4.octets()[2],
                        ip4.octets()[3],
                        (p.port() >> 8) as u8,
                        p.port() as u8,
                    ]),
                    std::net::IpAddr::V6(ip6) => {
                        let mut buf = vec![0u8; 18];
                        buf[..16].copy_from_slice(&ip6.octets());
                        buf[16..18].copy_from_slice(&p.port().to_be_bytes());
                        BencodeValue::Bytes(buf)
                    }
                })
                .collect();
            r_dict.insert(b"values".to_vec(), BencodeValue::List(peer_values));
        }

        if !nodes.is_empty() {
            let mut node_data = Vec::with_capacity(nodes.len() * 26);
            for (addr, nid) in nodes {
                node_data.extend_from_slice(nid);
                match addr.ip() {
                    std::net::IpAddr::V4(ip4) => {
                        node_data.extend_from_slice(&ip4.octets());
                        node_data.extend_from_slice(&(addr.port()).to_be_bytes());
                    }
                    std::net::IpAddr::V6(ip6) => {
                        node_data.extend_from_slice(&ip6.octets());
                        node_data.extend_from_slice(&(addr.port()).to_be_bytes());
                    }
                }
            }
            r_dict.insert(b"nodes".to_vec(), BencodeValue::Bytes(node_data));
        }

        DhtMessage::new_response(tx_id.to_vec(), BencodeValue::Dict(r_dict))
    }

    fn find_node_response(tx_id: &[u8], nodes: &[(SocketAddr, [u8; 20])]) -> DhtMessage {
        let mut r_dict = BTreeMap::new();
        r_dict.insert(b"id".to_vec(), BencodeValue::Bytes(vec![0x42u8; 20]));

        let mut node_data = Vec::with_capacity(nodes.len() * 38);
        for (addr, nid) in nodes {
            node_data.extend_from_slice(nid);
            match addr.ip() {
                std::net::IpAddr::V4(ip4) => {
                    node_data.extend_from_slice(&ip4.octets());
                    node_data.extend_from_slice(&(addr.port()).to_be_bytes());
                }
                std::net::IpAddr::V6(ip6) => {
                    node_data.extend_from_slice(&ip6.octets());
                    node_data.extend_from_slice(&(addr.port()).to_be_bytes());
                }
            }
        }
        r_dict.insert(b"nodes".to_vec(), BencodeValue::Bytes(node_data));

        DhtMessage::new_response(tx_id.to_vec(), BencodeValue::Dict(r_dict))
    }

    pub async fn expect_ping(&self) {
        let mut exp = self.expectations.lock().await;
        exp.insert(
            "ping".to_string(),
            DhtExpectation {
                response: None,
                peers: vec![],
                nodes: vec![],
            },
        );
    }

    pub async fn expect_get_peers(
        &self,
        peers: Vec<SocketAddr>,
        nodes: Vec<(SocketAddr, [u8; 20])>,
    ) {
        let mut exp = self.expectations.lock().await;
        exp.insert(
            "get_peers".to_string(),
            DhtExpectation {
                response: None,
                peers,
                nodes,
            },
        );
    }

    pub async fn expect_find_node(&self, nodes: Vec<(SocketAddr, [u8; 20])>) {
        let mut exp = self.expectations.lock().await;
        exp.insert(
            "find_node".to_string(),
            DhtExpectation {
                response: None,
                peers: vec![],
                nodes,
            },
        );
    }

    pub async fn expect_announce_peer(&self) {
        let mut exp = self.expectations.lock().await;
        exp.insert(
            "announce_peer".to_string(),
            DhtExpectation {
                response: None,
                peers: vec![],
                nodes: vec![],
            },
        );
    }

    pub async fn received_count(&self) -> usize {
        self.received_messages.lock().await.len()
    }

    pub async fn received_message(&self, index: usize) -> Option<DhtMessage> {
        let msgs = self.received_messages.lock().await;
        msgs.get(index).cloned()
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub async fn shutdown(self) {
        self.shutdown.notify_waiters();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_bind_and_addr() {
        let server = MockDhtServer::bind(0).await.unwrap();
        let addr = server.addr();
        assert!(addr.port() > 0);
        assert_eq!(addr.ip(), std::net::Ipv4Addr::new(127, 0, 0, 1));
        server.shutdown().await;
    }

    #[tokio::test]
    async fn test_responds_to_ping() {
        let server = MockDhtServer::bind(0).await.unwrap();
        server.expect_ping().await;

        use aria2_protocol::bittorrent::dht::socket::DhtSocket;
        let client = DhtSocket::bind(0).await.unwrap();
        let ping_msg = DhtMessageBuilder::ping(1, &[1u8; 20]);
        let encoded = ping_msg.encode().unwrap();

        client.send_to(server.addr(), &encoded).await.unwrap();

        let mut buf = [0u8; 512];
        let (n, _) = client
            .recv_with_timeout(&mut buf, std::time::Duration::from_secs(2))
            .await
            .unwrap();
        assert!(n > 0);

        let resp = DhtMessage::decode(&buf[..n]).unwrap();
        assert!(resp.is_response());

        server.shutdown().await;
    }

    #[tokio::test]
    async fn test_responds_to_get_peers_with_peers() {
        let server = MockDhtServer::bind(0).await.unwrap();
        let peers = vec!["10.0.0.1:6881".parse::<SocketAddr>().unwrap()];
        server.expect_get_peers(peers, vec![]).await;

        use aria2_protocol::bittorrent::dht::socket::DhtSocket;
        let client = DhtSocket::bind(0).await.unwrap();
        let query = DhtMessageBuilder::get_peers(1, &[1u8; 20], &[0xABu8; 20]);
        let encoded = query.encode().unwrap();

        client.send_to(server.addr(), &encoded).await.unwrap();

        let mut buf = [0u8; 1024];
        let (n, _) = client
            .recv_with_timeout(&mut buf, std::time::Duration::from_secs(2))
            .await
            .unwrap();
        let resp = DhtMessage::decode(&buf[..n]).unwrap();
        assert!(resp.is_response());

        let extracted =
            aria2_protocol::bittorrent::dht::client::extract_compact_peers_from_response(&resp);
        assert_eq!(extracted.len(), 1);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn test_tracks_received_messages() {
        let server = MockDhtServer::bind(0).await.unwrap();
        server.expect_ping().await;

        use aria2_protocol::bittorrent::dht::socket::DhtSocket;
        let client = DhtSocket::bind(0).await.unwrap();
        let ping_msg = DhtMessageBuilder::ping(99, &[2u8; 20]);
        let encoded = ping_msg.encode().unwrap();
        client.send_to(server.addr(), &encoded).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(server.received_count().await, 1);
        let msg = server.received_message(0).await.unwrap();
        assert!(msg.is_query());

        server.shutdown().await;
    }
}
