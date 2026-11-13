use std::sync::Once;

/// Install rustls' ring provider once before a protocol transport builds TLS.
///
/// Another crate may have installed a provider first; in that case rustls
/// rejects this installation and the caller keeps using the existing provider.
pub(crate) fn ensure_ring_provider() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
