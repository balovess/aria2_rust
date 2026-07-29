//! BitTorrent Web-seed (HTTP/HTTPS fallback for piece downloads)
//!
//! This module implements BEP 19 / BEP 17 Web Seed support, allowing
//! aria2-rust to download torrent pieces via HTTP Range requests when
//! the peer swarm is insufficient or unavailable.
//!
//! # Architecture
//!
//! - [`WebSeedClient`] - Single HTTP endpoint for downloading pieces
//! - [`WebSeedManager`] - Manages multiple web-seed URLs with fallback logic
//! - [`WebSeedStats`] - Speed statistics for web-seed downloads
//! - [`parse_url_list()`] - Extracts `url-list` from torrent metadata

mod client;
mod manager;
mod stats;
mod url_parser;

#[cfg(test)]
mod tests;

pub use client::WebSeedClient;
pub use manager::WebSeedManager;
pub use stats::WebSeedStats;
pub use url_parser::{parse_url_list, parse_url_list_from_bytes};
