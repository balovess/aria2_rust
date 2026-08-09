//! E2E Test Helpers
//!
//! Provides mock servers and test utilities for end-to-end testing.

pub mod mock_http_server;
pub mod mock_torrent;

use std::sync::Once;

static CRYPTO_PROVIDER_INIT: Once = Once::new();

/// Install the ring crypto provider for rustls.
///
/// Required when reqwest is built with the `rustls-no-provider` feature.
/// Must be called before constructing any `reqwest::Client` in tests.
/// Uses `Once` so it is safe to call multiple times.
///
/// Note: `install_default()` returns Err if a provider is already installed
/// (e.g., by aria2-protocol's `ensure_ring_provider()`). That is fine — we
/// only need to ensure a provider is present, not that we installed it.
pub fn ensure_crypto_provider() {
    CRYPTO_PROVIDER_INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
