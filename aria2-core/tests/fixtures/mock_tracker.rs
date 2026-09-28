use std::collections::BTreeMap;
use std::net::SocketAddr;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, watch};

#[derive(Clone)]
struct TrackerResponseConfig {
    peer_ports: Vec<u16>,
    fail_requests: bool,
    fail_after_requests: Option<usize>,
    interval_secs: u64,
    announce_list: Vec<Vec<String>>,
}

#[allow(dead_code)]
pub struct MockTrackerServer {
    addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    captured_queries: std::sync::Arc<Mutex<Vec<String>>>,
    query_count: watch::Sender<usize>,
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
        Self::start_with_peers_and_interval(peer_ports, fail_requests, 300).await
    }

    pub async fn start_with_peers_and_interval(
        peer_ports: Vec<u16>,
        fail_requests: bool,
        interval_secs: u64,
    ) -> Self {
        Self::start_with_response(peer_ports, fail_requests, interval_secs, Vec::new(), None).await
    }

    pub async fn start_with_dynamic_announce_list(
        peer_ports: Vec<u16>,
        interval_secs: u64,
        announce_list: Vec<Vec<String>>,
        fail_after_requests: Option<usize>,
    ) -> Self {
        Self::start_with_response(
            peer_ports,
            false,
            interval_secs,
            announce_list,
            fail_after_requests,
        )
        .await
    }

    async fn start_with_response(
        peer_ports: Vec<u16>,
        fail_requests: bool,
        interval_secs: u64,
        announce_list: Vec<Vec<String>>,
        fail_after_requests: Option<usize>,
    ) -> Self {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = TcpListener::bind(addr)
            .await
            .expect("Failed to bind mock tracker port");
        let actual_addr = listener.local_addr().unwrap();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        let captured_queries = std::sync::Arc::new(Mutex::new(Vec::new()));
        let (query_count, _) = watch::channel(0usize);

        let response_config = TrackerResponseConfig {
            peer_ports: peer_ports.clone(),
            fail_requests,
            fail_after_requests,
            interval_secs,
            announce_list,
        };
        let captured_queries_for_task = std::sync::Arc::clone(&captured_queries);
        let query_count_for_task = query_count.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((stream, _)) => {
                                let response_config = response_config.clone();
                                let captured_queries = std::sync::Arc::clone(&captured_queries_for_task);
                                let query_count = query_count_for_task.clone();
                                tokio::spawn(async move {
                                    Self::handle_connection(
                                        stream,
                                        response_config,
                                        captured_queries,
                                        query_count,
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
            query_count,
            fail_requests,
            _peer_port: peer_ports.first().copied().unwrap_or_default(),
            peer_ports,
        }
    }

    pub async fn captured_queries(&self) -> Vec<String> {
        self.captured_queries.lock().await.clone()
    }

    pub async fn wait_for_event(&self, event: &str) {
        let mut query_count = self.query_count.subscribe();
        let observed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if self
                    .captured_queries()
                    .await
                    .iter()
                    .any(|query| query.contains(&format!("event={event}")))
                {
                    return true;
                }
                if query_count.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(observed, "tracker did not receive event={event}");
    }

    pub async fn wait_for_query_count(
        &self,
        expected: usize,
        timeout: std::time::Duration,
    ) -> bool {
        let mut query_count = self.query_count.subscribe();
        tokio::time::timeout(timeout, async move {
            loop {
                if *query_count.borrow_and_update() >= expected {
                    return true;
                }
                if query_count.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await
        .unwrap_or(false)
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
        response_config: TrackerResponseConfig,
        captured_queries: std::sync::Arc<Mutex<Vec<String>>>,
        query_count: watch::Sender<usize>,
    ) {
        let mut reader = tokio::io::BufReader::new(&mut stream);

        let mut request_line = String::new();
        if reader.read_line(&mut request_line).await.is_err() {
            return;
        }
        let count = if let Some(path) = request_line
            .strip_prefix("GET ")
            .and_then(|path| path.split_whitespace().next())
        {
            let count = {
                let mut queries = captured_queries.lock().await;
                queries.push(path.to_string());
                queries.len()
            };
            query_count.send_replace(count);
            count
        } else {
            return;
        };

        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.is_err() {
                return;
            }
            if line == "\r\n" || line == "\n" || line.is_empty() {
                break;
            }
        }

        if response_config.fail_requests
            || response_config
                .fail_after_requests
                .is_some_and(|fail_at| count >= fail_at)
        {
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

        let body = build_tracker_response_bencode(
            &response_config.peer_ports,
            response_config.interval_secs,
            &response_config.announce_list,
        );

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
fn build_tracker_response_bencode(
    peer_ports: &[u16],
    interval_secs: u64,
    announce_list: &[Vec<String>],
) -> Vec<u8> {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;

    let compact_peers: Vec<u8> = peer_ports
        .iter()
        .flat_map(|peer_port| [127, 0, 0, 1, (*peer_port >> 8) as u8, *peer_port as u8])
        .collect();

    let mut resp_dict = BTreeMap::new();
    resp_dict.insert(
        b"interval".to_vec(),
        BencodeValue::Int(interval_secs as i64),
    );
    resp_dict.insert(b"complete".to_vec(), BencodeValue::Int(1));
    resp_dict.insert(b"incomplete".to_vec(), BencodeValue::Int(1));
    resp_dict.insert(b"peers".to_vec(), BencodeValue::Bytes(compact_peers));
    if !announce_list.is_empty() {
        resp_dict.insert(
            b"announce-list".to_vec(),
            BencodeValue::List(
                announce_list
                    .iter()
                    .map(|tier| {
                        BencodeValue::List(
                            tier.iter()
                                .map(|url| BencodeValue::Bytes(url.as_bytes().to_vec()))
                                .collect(),
                        )
                    })
                    .collect(),
            ),
        );
    }

    BencodeValue::Dict(resp_dict).encode()
}

impl Drop for MockTrackerServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}
