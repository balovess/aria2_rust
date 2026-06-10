// HTTP client pool for connection reuse across multiple downloads.
//
// Provides a singleton HTTP client that can be shared across multiple
// DownloadCommand instances to reduce connection establishment overhead
// and improve memory efficiency.

use once_cell::sync::Lazy;
use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;

/// Global HTTP client instance for connection reuse.
static GLOBAL_CLIENT: Lazy<Arc<Client>> = Lazy::new(|| {
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .user_agent("aria2-rust/1.0")
        .redirect(reqwest::redirect::Policy::limited(5))
        .pool_max_idle_per_host(16) // Increased from 8 for better concurrency
        .pool_idle_timeout(Some(Duration::from_secs(300))) // Increased from 90s
        .tcp_keepalive(Some(Duration::from_secs(60)))
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
    let client = Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(timeout)
        .user_agent("aria2-rust/1.0")
        .redirect(reqwest::redirect::Policy::limited(5))
        .pool_max_idle_per_host(pool_max_idle_per_host)
        .pool_idle_timeout(Some(Duration::from_secs(300)))
        .tcp_keepalive(Some(Duration::from_secs(60)))
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
        let custom = create_custom_client(
            Duration::from_secs(10),
            Duration::from_secs(60),
            8,
        );

        // Should be different instances
        assert!(!Arc::ptr_eq(&global, &custom));
    }
}
