//! Public identity values defined by the aria2 compatibility contract.
//!
//! These constants deliberately describe the upstream wire identity, not the
//! Rust workspace release. Existing clients and remote peers observe these
//! values through CLI, RPC, HTTP, and BitTorrent messages.

/// Version reported by aria2 1.37.0 to compatible external clients.
pub const ARIA2_VERSION: &str = "1.37.0";

/// Default HTTP User-Agent used by upstream aria2.
pub const DEFAULT_USER_AGENT: &str = "aria2/1.37.0";

/// Default BitTorrent extended-handshake peer agent used by upstream aria2.
pub const DEFAULT_PEER_AGENT: &str = DEFAULT_USER_AGENT;

/// Default BitTorrent peer-ID prefix generated from upstream's version.
pub const DEFAULT_PEER_ID_PREFIX: &str = "A2-1-37-0-";
