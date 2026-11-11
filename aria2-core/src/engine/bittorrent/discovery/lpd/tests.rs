//! Tests for Local Peer Discovery (LPD) Manager - Phase 15 H8
//!
//! Comprehensive tests covering:
//! - BEP 14 LPD announcement format validation
//! - Announcement parsing (valid, invalid, edge cases)
//! - Legacy format backward compatibility
//! - Duplicate suppression
//! - LpdPeer equality and hashing
//! - LpdManager lifecycle operations
//! - Private address detection

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use super::{LpdAnnouncer, LpdManager, LpdPeer, is_private_address, parse_lpd_announcement};

// =========================================================================
// Helper Functions
// =========================================================================

/// Create a valid 40-character hex info hash for testing
fn test_info_hash() -> &'static str {
    "0123456789abcdef0123456789abcdef01234567"
}

/// Alternative info hash for multi-hash tests
fn test_info_hash_2() -> &'static str {
    "fedcba9876543210fedcba9876543210fedcba98"
}

/// Test IP address
fn test_ip() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100))
}

/// Build a BEP 14 compliant LPD message
fn make_bep14_message(info_hash: &str, port: u16) -> Vec<u8> {
    format!(
        "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: {}\r\nInfohash: {}\r\n\r\n\r\n",
        port, info_hash
    )
    .into_bytes()
}

mod manager;
mod peer;
mod wire;
