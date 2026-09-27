// HTTP client pool for connection reuse across multiple downloads.
//
// Provides a singleton HTTP client that can be shared across multiple
// DownloadCommand instances to reduce connection establishment overhead
// and improve memory efficiency.

use once_cell::sync::Lazy;
use reqwest::Client;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::Once;
use std::time::Duration;

use dashmap::DashMap;

const HTTP2_DOWNLOAD_STREAM_WINDOW_SIZE: u32 = 16 * 1024 * 1024;
const HTTP2_DOWNLOAD_CONNECTION_WINDOW_SIZE: u32 = 16 * 1024 * 1024;
pub(crate) const HTTP2_DOWNLOAD_SESSION_COUNT: usize = 4;

/// Configure receive flow control for HTTP/2 response bodies.
///
/// reqwest's default 64 KiB stream and connection windows throttle large
/// downloads on higher-latency links. Keep the connection window bounded at
/// 16 MiB while giving each response stream enough room to keep the transport
/// busy. Measurements against the issue's GitHub asset show that increasing
/// only one window does not remove the throughput limit.
pub(crate) fn configure_http2_download_client(
    builder: reqwest::ClientBuilder,
) -> reqwest::ClientBuilder {
    builder
        .http2_initial_stream_window_size(Some(HTTP2_DOWNLOAD_STREAM_WINDOW_SIZE))
        .http2_initial_connection_window_size(Some(HTTP2_DOWNLOAD_CONNECTION_WINDOW_SIZE))
}

/// Ensure the rustls ring crypto provider is installed.
///
/// Required when reqwest is built with `rustls-no-provider` (no aws-lc-rs).
/// Must be called before any `reqwest::Client` is constructed.
///
/// Note: `install_default()` returns Err if a provider is already installed
/// (e.g., by another module's initializer). That is fine — we only need to
/// ensure a provider is present, not that we installed it.
pub fn ensure_rustls_provider() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Global HTTP client instance for connection reuse.
///
/// Redirects are disabled (`Policy::none`) because we handle them
/// manually in the download flow, matching C++ aria2 behavior:
/// - Update the RequestGroup URI list with redirect targets
/// - Feed redirect results back to the URI selector
/// - Track redirect count per-request
/// - Apply method change rules (301/302/303 → GET; 307/308 → preserve)
static GLOBAL_CLIENT: Lazy<Arc<Client>> = Lazy::new(|| {
    ensure_rustls_provider();
    Arc::new(build_bound_client(None, false))
});

static GLOBAL_DOWNLOAD_CLIENTS: Lazy<Arc<Vec<Client>>> = Lazy::new(|| {
    let mut clients = Vec::with_capacity(HTTP2_DOWNLOAD_SESSION_COUNT);
    clients.push(GLOBAL_CLIENT.as_ref().clone());
    clients.extend((1..HTTP2_DOWNLOAD_SESSION_COUNT).map(|_| build_bound_client(None, false)));
    Arc::new(clients)
});

/// Get the global shared HTTP client instance.
///
/// This client is shared across all downloads, enabling:
/// - TCP connection reuse
/// - Reduced memory footprint
/// - Better performance for concurrent downloads
pub fn get_global_client() -> Arc<Client> {
    GLOBAL_CLIENT.clone()
}

type BoundClientKey = (Option<IpAddr>, bool);
type BoundClients = DashMap<BoundClientKey, Arc<Client>>;
type BoundDownloadClients = DashMap<BoundClientKey, Arc<Vec<Client>>>;

static BOUND_CLIENTS: Lazy<BoundClients> = Lazy::new(DashMap::new);
static BOUND_DOWNLOAD_CLIENTS: Lazy<BoundDownloadClients> = Lazy::new(DashMap::new);

/// Return a process-wide reusable reqwest client for a source address.
///
/// reqwest fixes `local_address` on a client, so each configured source gets
/// one lazily-created connection pool. Clients are reused by every download
/// using the same source and gzip mode.
pub fn get_bound_client(local_address: Option<IpAddr>, accept_gzip: bool) -> Arc<Client> {
    if local_address.is_none() && !accept_gzip {
        return get_global_client();
    }
    let key = (local_address, accept_gzip);
    if let Some(client) = BOUND_CLIENTS.get(&key) {
        return Arc::clone(client.value());
    }

    let client = Arc::new(build_bound_client(local_address, accept_gzip));
    let entry = BOUND_CLIENTS
        .entry(key)
        .or_insert_with(|| Arc::clone(&client));
    Arc::clone(entry.value())
}

/// Return independent HTTP clients for concurrent Range traffic. Each client
/// owns a separate HTTP/2 connection pool; the segment executor warms one
/// session per client before multiplexing additional streams onto it.
pub(crate) fn get_bound_download_clients(
    local_address: Option<IpAddr>,
    accept_gzip: bool,
) -> Arc<Vec<Client>> {
    if local_address.is_none() && !accept_gzip {
        return Arc::clone(&GLOBAL_DOWNLOAD_CLIENTS);
    }

    let key = (local_address, accept_gzip);
    if let Some(clients) = BOUND_DOWNLOAD_CLIENTS.get(&key) {
        return Arc::clone(clients.value());
    }

    let primary = get_bound_client(local_address, accept_gzip);
    let mut clients = Vec::with_capacity(HTTP2_DOWNLOAD_SESSION_COUNT);
    clients.push(primary.as_ref().clone());
    clients.extend(
        (1..HTTP2_DOWNLOAD_SESSION_COUNT).map(|_| build_bound_client(local_address, accept_gzip)),
    );
    let clients = Arc::new(clients);
    let entry = BOUND_DOWNLOAD_CLIENTS
        .entry(key)
        .or_insert_with(|| Arc::clone(&clients));
    Arc::clone(entry.value())
}

/// Create a custom HTTP client with specific configuration.
///
/// Use this when you need client settings different from the global defaults.
/// Redirects are disabled — they are handled manually in the download flow
/// matching C++ aria2 behavior.
pub fn create_custom_client(
    connect_timeout: Duration,
    timeout: Duration,
    pool_max_idle_per_host: usize,
) -> Arc<Client> {
    ensure_rustls_provider();
    let builder = Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(timeout)
        .gzip(false)
        .user_agent(crate::constants::USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(pool_max_idle_per_host)
        .pool_idle_timeout(Some(Duration::from_secs(
            crate::constants::HTTP_CLIENT_POOL_IDLE_TIMEOUT_SECS,
        )))
        .tcp_keepalive(Some(Duration::from_secs(
            crate::constants::HTTP_DEFAULT_TCP_KEEPALIVE_SECS,
        )))
        .tcp_nodelay(true);
    let client = configure_http2_download_client(builder)
        .build()
        .expect("Failed to create custom HTTP client");

    Arc::new(client)
}

fn build_bound_client(local_address: Option<IpAddr>, accept_gzip: bool) -> Client {
    ensure_rustls_provider();
    let mut builder = Client::builder()
        .connect_timeout(Duration::from_secs(
            crate::constants::HTTP_DEFAULT_CONNECT_TIMEOUT_SECS,
        ))
        .gzip(accept_gzip)
        .user_agent(crate::constants::USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(crate::constants::HTTP_CLIENT_POOL_MAX_IDLE_PER_HOST)
        .pool_idle_timeout(Some(Duration::from_secs(
            crate::constants::HTTP_CLIENT_POOL_IDLE_TIMEOUT_SECS,
        )))
        .tcp_keepalive(Some(Duration::from_secs(
            crate::constants::HTTP_DEFAULT_TCP_KEEPALIVE_SECS,
        )))
        .tcp_nodelay(true);
    if let Some(address) = local_address {
        builder = builder.local_address(address);
    }
    configure_http2_download_client(builder)
        .build()
        .expect("bound HTTP client construction should be infallible")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_global_client_is_shared() {
        let client1 = get_global_client();
        let client2 = get_global_client();

        // Both should point to the same client instance
        assert!(Arc::ptr_eq(&client1, &client2));
    }

    #[test]
    fn test_custom_client_is_different() {
        let global = get_global_client();
        let custom = create_custom_client(Duration::from_secs(10), Duration::from_secs(60), 8);

        // Should be different instances
        assert!(!Arc::ptr_eq(&global, &custom));
    }

    #[test]
    fn bound_clients_are_reused_per_source_and_gzip_mode() {
        let source = Some("127.0.0.1".parse().unwrap());
        let first = get_bound_client(source, false);
        let second = get_bound_client(source, false);
        let gzip = get_bound_client(source, true);
        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &gzip));
    }

    #[tokio::test]
    async fn bound_client_reuses_one_real_tcp_connection() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let source = "127.0.0.3".parse().unwrap();
        let listener = TcpListener::bind((source, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, peer) = listener.accept().await.unwrap();
            for _ in 0..2 {
                let mut request = Vec::new();
                loop {
                    let mut byte = [0u8; 1];
                    stream.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok",
                    )
                    .await
                    .unwrap();
            }
            peer
        });

        let client = get_bound_client(Some(source), false);
        for path in ["/one", "/two"] {
            let response = client
                .get(format!("http://{address}{path}"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.text().await.unwrap(), "ok");
        }
        assert_eq!(server.await.unwrap().ip(), source);
    }

    #[tokio::test]
    async fn configured_download_client_receives_concurrent_http2_ranges() {
        use std::convert::Infallible;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        use bytes::Bytes;
        use http_body_util::Full;
        use hyper::{
            Request, Response,
            body::Incoming,
            header::{CONTENT_RANGE, RANGE},
            server::conn::http2,
            service::service_fn,
        };
        use hyper_util::rt::{TokioExecutor, TokioIo};
        use tokio::net::TcpListener;

        const SESSION_COUNT: usize = 4;
        const SEGMENT_COUNT: usize = 16;
        const SEGMENT_SIZE: usize = 1024 * 1024;
        const TOTAL_SIZE: usize = SEGMENT_COUNT * SEGMENT_SIZE;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let expected = Bytes::from((0..TOTAL_SIZE).map(|i| (i % 251) as u8).collect::<Vec<_>>());
        let server_expected = expected.clone();
        let served_per_connection = Arc::new(
            (0..SESSION_COUNT)
                .map(|_| AtomicUsize::new(0))
                .collect::<Vec<_>>(),
        );
        let server_served_per_connection = Arc::clone(&served_per_connection);
        let accepted_connections = Arc::new(AtomicUsize::new(0));
        let server_accepted_connections = Arc::clone(&accepted_connections);
        let server = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            for connection_index in 0..SESSION_COUNT {
                let (stream, _) = listener.accept().await.unwrap();
                server_accepted_connections.fetch_add(1, AtomicOrdering::Relaxed);
                let body = server_expected.clone();
                let served_per_connection = Arc::clone(&server_served_per_connection);
                let service = service_fn(move |request: Request<Incoming>| {
                    let body = body.clone();
                    let served_per_connection = Arc::clone(&served_per_connection);
                    async move {
                        let index = request
                            .uri()
                            .path()
                            .rsplit('/')
                            .next()
                            .unwrap()
                            .parse::<usize>()
                            .unwrap();
                        let start = index * SEGMENT_SIZE;
                        let end = start + SEGMENT_SIZE - 1;
                        let expected_range = format!("bytes={start}-{end}");
                        assert_eq!(
                            request.headers().get(RANGE).unwrap(),
                            &expected_range,
                            "each HTTP/2 stream should carry its assigned byte range"
                        );
                        served_per_connection[connection_index]
                            .fetch_add(1, AtomicOrdering::Relaxed);
                        let response_body = body.slice(start..end + 1);
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(reqwest::StatusCode::PARTIAL_CONTENT)
                                .header(CONTENT_RANGE, format!("bytes {start}-{end}/{TOTAL_SIZE}"))
                                .body(Full::new(response_body))
                                .unwrap(),
                        )
                    }
                });
                connections.spawn(async move {
                    http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                        .unwrap();
                });
            }
            while let Some(connection) = connections.join_next().await {
                connection.unwrap();
            }
        });

        ensure_rustls_provider();
        let clients = (0..SESSION_COUNT)
            .map(|_| {
                configure_http2_download_client(Client::builder().http2_prior_knowledge())
                    .build()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let initial_responses = futures::future::join_all((0..SESSION_COUNT).map(|index| {
            let client = clients[index].clone();
            async move {
                let start = index * SEGMENT_SIZE;
                let end = start + SEGMENT_SIZE - 1;
                let response = client
                    .get(format!("http://{address}/segment/{index}"))
                    .header(RANGE, format!("bytes={start}-{end}"))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.version(), reqwest::Version::HTTP_2);
                assert_eq!(response.status(), reqwest::StatusCode::PARTIAL_CONTENT);
                response
            }
        }))
        .await;

        let transfers = (SESSION_COUNT..SEGMENT_COUNT).map(|index| {
            let client = clients[index % SESSION_COUNT].clone();
            let expected = expected.clone();
            async move {
                let start = index * SEGMENT_SIZE;
                let end = start + SEGMENT_SIZE - 1;
                let response = client
                    .get(format!("http://{address}/segment/{index}"))
                    .header(RANGE, format!("bytes={start}-{end}"))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.version(), reqwest::Version::HTTP_2);
                assert_eq!(response.status(), reqwest::StatusCode::PARTIAL_CONTENT);
                let received = response.bytes().await.unwrap();
                assert_eq!(received.as_ref(), &expected[start..=end]);
            }
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            futures::future::join_all(transfers),
        )
        .await
        .expect("concurrent HTTP/2 range responses should not stall");
        for (index, response) in initial_responses.into_iter().enumerate() {
            let start = index * SEGMENT_SIZE;
            let end = start + SEGMENT_SIZE;
            let received = response.bytes().await.unwrap();
            assert_eq!(received.as_ref(), &expected[start..end]);
        }
        drop(clients);
        tokio::time::timeout(std::time::Duration::from_secs(10), server)
            .await
            .expect("HTTP/2 sessions should shut down after clients are dropped")
            .unwrap();
        assert_eq!(
            accepted_connections.load(AtomicOrdering::Relaxed),
            SESSION_COUNT,
            "four independent clients should establish exactly four TCP sessions"
        );
        assert_eq!(
            served_per_connection
                .iter()
                .map(|count| count.load(AtomicOrdering::Relaxed))
                .collect::<Vec<_>>(),
            vec![SEGMENT_COUNT / SESSION_COUNT; SESSION_COUNT],
            "each HTTP/2 TCP session should multiplex four Range streams"
        );
    }
}
