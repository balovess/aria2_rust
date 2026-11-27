//! # aria2-protocol
//!
//! Protocol implementations for the aria2-rust download utility.
//!
//! Provides reusable protocol implementations and protocol-level utilities
//! for HTTP/HTTPS, HLS playlist syntax, FTP, SFTP, BitTorrent, and Metalink. HTTP, FTP, SOCKS,
//! proxy URL parsing, URI component encoding, and identity defaults are always
//! available; BitTorrent, Metalink, and SFTP are enabled by their respective
//! crate features. The download engine owns task policy, network routing, and
//! process-wide lifecycle; these modules expose protocol operations without
//! depending on the engine.
//!
//! ## Modules
//!
//! - **[`http`]** — Standalone HTTP/HTTPS client built on reqwest, plus reusable
//!   request serialization, response parsing, authentication, CONNECT, cookie,
//!   content-decoding, redirect, and header-parsing operations. The standalone
//!   `HttpClient` does not configure proxies; the download engine composes proxy
//!   and network policy around its task-facing clients.
//!
//! - **[`hls`]** — HLS/M3U8 media-playlist parsing and segment URL resolution.
//!
//! - **[`ftp`]** — FTP client: control connection (USER/PASS/CWD/SIZE/PASV/EPSV),
//!   passive mode data transfer, anonymous and authenticated login, REST-based resume.
//!
//! - **[`bittorrent`]** *(feature: `bittorrent`)* — Full BitTorrent protocol stack:
//!   bencode codec, .torrent parsing, info_hash computation, BT message protocol (10 types),
//!   handshake, incoming TCP listener and handshake primitives, Tracker
//!   communication (HTTP announce/scrape), DHT network (K-buckets/KRPC), PEX
//!   peer exchange, MSE encryption framework, and `PeerConnection`. The engine
//!   composes the raw listener with torrent routing, admission, and lifecycle
//!   management.
//!
//! - **[`metalink`]** *(feature: `metalink`)* — Metalink V3/V4 XML parser:
//!   multi-mirror URL priority, hash verification (MD5/SHA1/SHA256/SHA512),
//!   piece checksums, MetaURL torrent detection.
//!
//! - **[`sftp`]** *(feature: `sftp`)* — SFTP over SSH2 client: key/password auth,
//!   file operations (stat/read/write/mkdir/rmdir/readdir/symlink), streaming transfer.
//!
//! - **[`socks`]** — SOCKS4/SOCKS5 CONNECT handshakes over caller-provided
//!   streams. Socket creation and target-address policy belong to the caller.
//!
//! - **[`proxy`]** — Proxy URL parsing, including percent-decoded credentials;
//!   it does not resolve hosts or select routes.
//!
//! - **[`uri`]** — URI component percent-encoding and decoding helpers.
//!
//! - **[`identity`]** — Default product and peer identity strings for standalone
//!   library users. Applications can supply their own identity through the
//!   higher-level configuration boundary.

pub mod ftp;
pub mod hls;
pub mod http;
pub mod identity;
pub mod proxy;
pub mod socks;
pub mod uri;

mod crypto;

/// Default TCP connection timeout for SFTP's SSH transport, in seconds.
pub const DEFAULT_SFTP_CONNECT_TIMEOUT_SECS: u64 = 15;
/// Default TCP port for SSH/SFTP connections.
pub const DEFAULT_SFTP_PORT: u16 = 22;
/// Default SFTP channel read timeout, in seconds.
pub const DEFAULT_SFTP_READ_TIMEOUT_SECS: u64 = 30;
/// Default SSH keepalive interval for SFTP transport, in seconds.
pub const DEFAULT_SFTP_KEEPALIVE_INTERVAL_SECS: u64 = 60;
/// Standard BitTorrent Local Peer Discovery multicast address.
pub const DEFAULT_LPD_MULTICAST_ADDRESS: &str = "239.192.152.143";
/// Standard BitTorrent Local Peer Discovery UDP port.
pub const DEFAULT_LPD_PORT: u16 = 6771;

#[cfg(feature = "bittorrent")]
pub mod bittorrent;

#[cfg(feature = "metalink")]
pub mod metalink;

#[cfg(feature = "sftp")]
pub mod sftp;
