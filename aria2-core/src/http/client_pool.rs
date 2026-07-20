// HTTP client pool for connection reuse across multiple downloads.
//
// Provides a singleton HTTP client that can be shared across multiple
// DownloadCommand instances to reduce connection establishment overhead
// and improve memory efficiency.

use once_cell::sync::Lazy;
use reqwest::Client;
use std::sync::Arc;
use std::sync::Once;
use std::time::Duration;

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
static GLOBAL_CLIENT: Lazy<Arc<Client>> = Lazy::new(|| {
    ensure_rustls_provider();
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(
            crate::constants::HTTP_DEFAULT_CONNECT_TIMEOUT_SECS,
        ))
        .timeout(Duration::from_secs(
            crate::constants::HTTP_DEFAULT_OVERALL_TIMEOUT_SECS,
        ))
        .user_agent(crate::constants::USER_AGENT)
        .redirect(reqwest::redirect::Policy::limited(
            crate::constants::HTTP_DEFAULT_MAX_REDIRECTS,
        ))
        .pool_max_idle_per_host(crate::constants::HTTP_CLIENT_POOL_MAX_IDLE_PER_HOST)
        .pool_idle_timeout(Some(Duration::from_secs(
            crate::constants::HTTP_CLIENT_POOL_IDLE_TIMEOUT_SECS,
        )))
        .tcp_keepalive(Some(Duration::from_secs(
            crate::constants::HTTP_DEFAULT_TCP_KEEPALIVE_SECS,
        )))
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

/// Create a custom HTTP client with specific configuration.
///
/// Use this when you need client settings different from the global defaults.
pub fn create_custom_client(
    connect_timeout: Duration,
    timeout: Duration,
    pool_max_idle_per_host: usize,
) -> Arc<Client> {
    ensure_rustls_provider();
    let client = Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(timeout)
        .user_agent(crate::constants::USER_AGENT)
        .redirect(reqwest::redirect::Policy::limited(
            crate::constants::HTTP_DEFAULT_MAX_REDIRECTS,
        ))
        .pool_max_idle_per_host(pool_max_idle_per_host)
        .pool_idle_timeout(Some(Duration::from_secs(
            crate::constants::HTTP_CLIENT_POOL_IDLE_TIMEOUT_SECS,
        )))
        .tcp_keepalive(Some(Duration::from_secs(
            crate::constants::HTTP_DEFAULT_TCP_KEEPALIVE_SECS,
        )))
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
}
