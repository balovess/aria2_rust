//! # aria2-protocol
//!
//! Protocol implementations for the aria2-rust download utility.
//!
//! Provides client-side protocol handlers for HTTP/HTTPS, FTP/SFTP, BitTorrent,
//! Metalink, and SFTP downloads. Each protocol module is feature-gated to allow
//! minimal builds when only specific protocols are needed.
//!
//! ## Modules
//!
//! - **[`http`]** — HTTP/HTTPS client built on reqwest/hyper: request construction,
//!   Range header handling, BASIC/Digest authentication, proxy support (HTTP/SOCKS5),
//!   gzip/deflate decompression, Cookie management, chunked transfer decoding,
//!   custom headers, and redirect following.
//!
//! - **[`ftp`]** — FTP/SFTP client: control connection (USER/PASS/CWD/SIZE/PASV/EPSV),
//!   passive mode data transfer, anonymous and authenticated login, REST-based resume.
//!
//! - **[`bittorrent`]** *(feature: `bittorrent`)* — Full BitTorrent protocol stack:
//!   bencode codec, .torrent parsing, info_hash computation, BT message protocol (10 types),
//!   handshake, Tracker communication (HTTP announce/scrape), DHT network (K-buckets/KRPC),
//!   PEX peer exchange, MSE encryption framework, PeerConnection, choke algorithm,
//!   PiecePicker (RarestFirst/Sequential/Random/EndGame).
//!
//! - **[`metalink`]** *(feature: `metalink`)* — Metalink V3/V4 XML parser:
//!   multi-mirror URL priority, hash verification (MD5/SHA1/SHA256/SHA512),
//!   piece checksums, MetaURL torrent detection.
//!
//! - **[`sftp`]` *(feature: `sftp`)* — SFTP over SSH2 client: key/password auth,
//!   file operations (stat/read/write/mkdir/rmdir/readdir/symlink), streaming transfer.

pub mod ftp;
pub mod http;

#[cfg(feature = "bittorrent")]
pub mod bittorrent;

#[cfg(feature = "metalink")]
pub mod metalink;

#[cfg(feature = "sftp")]
pub mod sftp;
