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
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(
            crate::constants::HTTP_DEFAULT_CONNECT_TIMEOUT_SECS,
        ))
        // reqwest enables gzip negotiation by default when its gzip feature
        // is compiled in. aria2 enables it only for --http-accept-gzip.
        .gzip(false)
        .user_agent(crate::constants::USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(crate::constants::HTTP_CLIENT_POOL_MAX_IDLE_PER_HOST)
        .pool_idle_timeout(Some(Duration::from_secs(
            crate::constants::HTTP_CLIENT_POOL_IDLE_TIMEOUT_SECS,
        )))
        .tcp_keepalive(Some(Duration::from_secs(
            crate::constants::HTTP_DEFAULT_TCP_KEEPALIVE_SECS,
        )))
        .tcp_nodelay(true)
        .build()
        .expect("Failed to create global HTTP client");

    Arc::new(client)
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

static BOUND_CLIENTS: Lazy<BoundClients> = Lazy::new(DashMap::new);

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
    let client = Arc::new(
        builder
            .build()
            .expect("bound HTTP client construction should be infallible"),
    );
    let entry = BOUND_CLIENTS
        .entry(key)
        .or_insert_with(|| Arc::clone(&client));
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
    let client = Client::builder()
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
        .tcp_nodelay(true)
        .build()
        .expect("Failed to create custom HTTP client");

    Arc::new(client)
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
}
