//! # aria2-protocol
//!
//! Protocol implementations for the aria2-rust download utility.
//!
//! Provides client-side protocol handlers for HTTP/HTTPS, FTP, SFTP, BitTorrent,
//! and Metalink. Optional protocol modules are feature-gated to allow minimal
//! builds when only specific protocols are needed.
//!
//! ## Modules
//!
//! - **[`http`]** — HTTP/HTTPS client built on reqwest/hyper: request construction,
//!   Range header handling, BASIC/Digest authentication, proxy support (HTTP/SOCKS5),
//!   gzip/deflate decompression, Cookie management, chunked transfer decoding,
//!   incremental HTTP/1.x response-header parsing, custom headers, and redirect
//!   following.
//!
//! - **[`ftp`]** — FTP client: control connection (USER/PASS/CWD/SIZE/PASV/EPSV),
//!   passive mode data transfer, anonymous and authenticated login, REST-based resume.
//!
//! - **[`bittorrent`]** *(feature: `bittorrent`)* — Full BitTorrent protocol stack:
//!   bencode codec, .torrent parsing, info_hash computation, BT message protocol (10 types),
//!   handshake, Tracker communication (HTTP announce/scrape), DHT network (K-buckets/KRPC),
//!   PEX peer exchange, MSE encryption framework, and PeerConnection.
//!
//! - **[`metalink`]** *(feature: `metalink`)* — Metalink V3/V4 XML parser:
//!   multi-mirror URL priority, hash verification (MD5/SHA1/SHA256/SHA512),
//!   piece checksums, MetaURL torrent detection.
//!
//! - **[`sftp`]** *(feature: `sftp`)* — SFTP over SSH2 client: key/password auth,
//!   file operations (stat/read/write/mkdir/rmdir/readdir/symlink), streaming transfer.

pub mod ftp;
pub mod http;
pub mod identity;
pub mod socks;

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
