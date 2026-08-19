//! Product identity values shared by public CLI, RPC, HTTP, and BitTorrent
//! adapters.
//!
//! The protocol shapes remain aria2-compatible, but every emitted product
//! version identifies this independent Rust implementation.

/// Product name used in user-visible identity strings.
pub const PRODUCT_NAME: &str = "aria2-rust";

/// Version of the `aria2-protocol` library package.
///
/// This is deliberately separate from the `aria2` binary release version.
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Compatibility alias for callers that used the old protocol identity API.
///
/// Binary product surfaces must use the `aria2` crate's identity instead.
pub const PRODUCT_VERSION: &str = PACKAGE_VERSION;

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
    fn library_identity_uses_the_package_version() {
        assert_eq!(PACKAGE_VERSION, env!("CARGO_PKG_VERSION"));
        assert_eq!(PRODUCT_VERSION, PACKAGE_VERSION);
        assert_eq!(
            DEFAULT_USER_AGENT,
            format!("{PRODUCT_NAME}/{PACKAGE_VERSION}")
        );
        assert_eq!(DEFAULT_PEER_AGENT, DEFAULT_USER_AGENT);
        assert!(DEFAULT_PEER_ID_PREFIX.starts_with("A2-RUST-"));
    }

    #[test]
    fn product_identity_does_not_impersonate_upstream_aria2() {
        assert_ne!(PRODUCT_NAME, "aria2");
        assert!(!DEFAULT_USER_AGENT.starts_with("aria2/"));
        assert!(!DEFAULT_PEER_AGENT.starts_with("aria2/"));
    }

    #[test]
    fn library_identity_is_not_the_binary_release_source() {
        assert_eq!(PACKAGE_VERSION, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            DEFAULT_USER_AGENT,
            "aria2-rust/".to_owned() + PACKAGE_VERSION
        );
        assert_eq!(DEFAULT_PEER_AGENT, DEFAULT_USER_AGENT);
    }
}
