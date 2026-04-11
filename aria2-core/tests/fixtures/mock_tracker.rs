use std::collections::BTreeMap;
use std::net::SocketAddr;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub struct MockTrackerServer {
    addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    _peer_port: u16,
}

impl MockTrackerServer {
    pub async fn start(peer_port: u16) -> Self {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = TcpListener::bind(addr)
            .await
            .expect("绑定Mock Tracker端口失败");
        let actual_addr = listener.local_addr().unwrap();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();

        let pp = peer_port;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((stream, _)) => {
                                let pp_inner = pp;
                                tokio::spawn(async move { Self::handle_connection(stream, pp_inner).await; });
                            }
                            Err(_) => break,
                        }
                    }
                    _ = &mut shutdown_rx => break,
                }
            }
        });

        MockTrackerServer {
            addr: actual_addr,
            shutdown: Some(shutdown_tx),
            _peer_port: peer_port,
        }
    }

    #[allow(dead_code)]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
    pub fn announce_url(&self) -> String {
        format!("http://127.0.0.1:{}/announce", self.addr.port())
    }

    async fn handle_connection(mut stream: tokio::net::TcpStream, peer_port: u16) {
        let mut reader = tokio::io::BufReader::new(&mut stream);

        let mut request_line = String::new();
        if reader.read_line(&mut request_line).await.is_err() {
            return;
        }
        if !request_line.starts_with("GET ") {
            return;
        }

        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.is_err() {
                return;
            }
            if line == "\r\n" || line == "\n" || line.is_empty() {
                break;
            }
        }

        let body = build_tracker_response_bencode(peer_port);

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );

        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
        if stream.write_all(&body).await.is_err() {
            return;
        }
        let _ = stream.flush().await;
        let _ = stream.shutdown().await;
    }
}

fn build_tracker_response_bencode(peer_port: u16) -> Vec<u8> {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;

    let mut peer_dict = BTreeMap::new();
    peer_dict.insert(b"ip".to_vec(), BencodeValue::Bytes(b"127.0.0.1".to_vec()));
    peer_dict.insert(b"port".to_vec(), BencodeValue::Int(peer_port as i64));

    let mut resp_dict = BTreeMap::new();
    resp_dict.insert(b"interval".to_vec(), BencodeValue::Int(300));
    resp_dict.insert(b"complete".to_vec(), BencodeValue::Int(1));
    resp_dict.insert(b"incomplete".to_vec(), BencodeValue::Int(1));
    resp_dict.insert(
        b"peers".to_vec(),
        BencodeValue::List(vec![BencodeValue::Dict(peer_dict)]),
    );

    BencodeValue::Dict(resp_dict).encode()
}

impl Drop for MockTrackerServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}
