use std::collections::BTreeMap;
use std::net::SocketAddr;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

#[allow(dead_code)]
pub struct MockTrackerServer {
    addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    captured_queries: std::sync::Arc<Mutex<Vec<String>>>,
    #[allow(dead_code)]
    fail_requests: bool,
    #[allow(dead_code)]
    _peer_port: u16,
    peer_ports: Vec<u16>,
}

#[allow(dead_code)]
impl MockTrackerServer {
    pub async fn start(peer_port: u16) -> Self {
        Self::start_with_failure(peer_port, false).await
    }

    pub async fn start_with_failure(peer_port: u16, fail_requests: bool) -> Self {
        Self::start_with_peers(vec![peer_port], fail_requests).await
    }

    pub async fn start_with_peers(peer_ports: Vec<u16>, fail_requests: bool) -> Self {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = TcpListener::bind(addr)
            .await
            .expect("Failed to bind mock tracker port");
        let actual_addr = listener.local_addr().unwrap();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        let captured_queries = std::sync::Arc::new(Mutex::new(Vec::new()));

        let pp = peer_ports.clone();
        let fail_requests_for_task = fail_requests;
        let captured_queries_for_task = std::sync::Arc::clone(&captured_queries);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((stream, _)) => {
                                let pp_inner = pp.clone();
                                let captured_queries = std::sync::Arc::clone(&captured_queries_for_task);
                                tokio::spawn(async move {
                                    Self::handle_connection(
                                        stream,
                                        pp_inner.as_slice(),
                                        fail_requests_for_task,
                                        captured_queries,
                                    )
                                    .await;
                                });
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
            captured_queries,
            fail_requests,
            _peer_port: peer_ports.first().copied().unwrap_or_default(),
            peer_ports,
        }
    }

    pub async fn captured_queries(&self) -> Vec<String> {
        self.captured_queries.lock().await.clone()
    }

    pub async fn wait_for_event(&self, event: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if self
                    .captured_queries()
                    .await
                    .iter()
                    .any(|query| query.contains(&format!("event={event}")))
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("tracker did not receive event={event}"));
    }

    #[allow(dead_code)]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
    pub fn announce_url(&self) -> String {
        format!("http://127.0.0.1:{}/announce", self.addr.port())
    }

    async fn handle_connection(
        mut stream: tokio::net::TcpStream,
        peer_ports: &[u16],
        fail_requests: bool,
        captured_queries: std::sync::Arc<Mutex<Vec<String>>>,
    ) {
        let mut reader = tokio::io::BufReader::new(&mut stream);

        let mut request_line = String::new();
        if reader.read_line(&mut request_line).await.is_err() {
            return;
        }
        if let Some(path) = request_line.strip_prefix("GET ")
            && let Some(path) = path.split_whitespace().next()
        {
            captured_queries.lock().await.push(path.to_string());
        } else {
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

        if fail_requests {
            let body = b"failure";
            let response = format!(
                "HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.write_all(body).await;
            let _ = stream.shutdown().await;
            return;
        }

        let body = build_tracker_response_bencode(peer_ports);

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

#[allow(dead_code)]
fn build_tracker_response_bencode(peer_ports: &[u16]) -> Vec<u8> {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;

    let compact_peers: Vec<u8> = peer_ports
        .iter()
        .flat_map(|peer_port| [127, 0, 0, 1, (*peer_port >> 8) as u8, *peer_port as u8])
        .collect();

    let mut resp_dict = BTreeMap::new();
    resp_dict.insert(b"interval".to_vec(), BencodeValue::Int(300));
    resp_dict.insert(b"complete".to_vec(), BencodeValue::Int(1));
    resp_dict.insert(b"incomplete".to_vec(), BencodeValue::Int(1));
    resp_dict.insert(b"peers".to_vec(), BencodeValue::Bytes(compact_peers));

    BencodeValue::Dict(resp_dict).encode()
}

impl Drop for MockTrackerServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}
