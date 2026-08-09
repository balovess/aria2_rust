//! Product identity values shared by public CLI, RPC, HTTP, and BitTorrent
//! adapters.
//!
//! The protocol shapes remain aria2-compatible, but every emitted product
//! version identifies this independent Rust implementation.

/// Product name used in user-visible identity strings.
pub const PRODUCT_NAME: &str = "aria2-rust";

/// Single release version source for all public adapters.
pub const PRODUCT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default HTTP User-Agent emitted by this implementation.
pub const DEFAULT_USER_AGENT: &str = concat!("aria2-rust/", env!("CARGO_PKG_VERSION"));

/// Default BitTorrent extended-handshake peer agent.
pub const DEFAULT_PEER_AGENT: &str = DEFAULT_USER_AGENT;

/// Default BitTorrent peer-ID prefix for this implementation.
pub const DEFAULT_PEER_ID_PREFIX: &str = "A2-RUST-";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_identity_uses_the_package_version() {
        assert_eq!(PRODUCT_VERSION, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            DEFAULT_USER_AGENT,
            format!("{PRODUCT_NAME}/{PRODUCT_VERSION}")
        );
        assert_eq!(DEFAULT_PEER_AGENT, DEFAULT_USER_AGENT);
        assert!(DEFAULT_PEER_ID_PREFIX.starts_with("A2-RUST-"));
    }
}
