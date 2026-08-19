//! Identity owned by the `aria2` binary product.

/// Product name used in user-visible identity strings.
pub const PRODUCT_NAME: &str = "aria2-rust";

/// Release version of the `aria2` binary package.
pub const PRODUCT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default HTTP User-Agent emitted by the binary product.
pub const DEFAULT_USER_AGENT: &str = concat!("aria2-rust/", env!("CARGO_PKG_VERSION"));

/// Default BitTorrent extended-handshake peer agent emitted by the binary.
pub const DEFAULT_PEER_AGENT: &str = DEFAULT_USER_AGENT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_version_comes_from_the_aria2_package() {
        assert_eq!(PRODUCT_VERSION, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            DEFAULT_USER_AGENT,
            format!("{PRODUCT_NAME}/{PRODUCT_VERSION}")
        );
        assert_eq!(DEFAULT_PEER_AGENT, DEFAULT_USER_AGENT);
    }
}
