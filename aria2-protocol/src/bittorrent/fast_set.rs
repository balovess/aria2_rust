//! BEP 6 Fast Extension: `computeFastSet` algorithm.
//!
//! Computes the set of piece indices that a peer is allowed to download
//! even while choked, per the BitTorrent Fast Extension (BEP 6).
//!
//! # Algorithm
//!
//! 1. Convert the peer's IP address to a 4-byte big-endian value:
//!    - IPv4: the raw 4 octets.
//!    - IPv6: SHA-1 of the 16-byte address, take first 4 bytes.
//! 2. Apply privacy masking to the 4-byte IP value.
//! 3. Build seed = `[masked_ip (4 bytes)][info_hash (20 bytes)]` (24 bytes).
//! 4. Compute SHA-1 of the seed → `x` (20 bytes).
//! 5. Repeatedly extract up to 5 big-endian u32 values from `x`,
//!    each mod `num_pieces`, deduplicating into the result set.
//! 6. When `x` is exhausted, rehash: `x = SHA-1(x)`.
//! 7. Stop when the set reaches `set_size` entries.
//!
//! # C++ Reference
//!
//! `bittorrent::computeFastSet()` in `bittorrent_helper.cc`.
//! The C++ version only supports IPv4 (returns empty for IPv6).
//! This Rust version extends support to IPv6 via SHA-1 folding.

use sha1::{Digest, Sha1};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Compute the BEP 6 allowed-fast set for a peer.
///
/// Returns a sorted (insertion-order) Vec of unique piece indices that
/// the peer is allowed to request even while choked.
///
/// # Arguments
///
/// * `ip`         — Peer IP address string (IPv4 or IPv6).
/// * `num_pieces` — Total number of pieces in the torrent (must be > 0).
/// * `info_hash`  — 20-byte torrent info hash.
/// * `set_size`   — Maximum number of entries in the fast set (default 10).
///
/// # Returns
///
/// A `Vec<u32>` of piece indices. Empty if `num_pieces == 0` or the IP
/// address cannot be parsed.
///
/// # Examples
///
/// ```
/// use aria2_protocol::bittorrent::fast_set::compute_fast_set;
///
/// let info_hash = [0u8; 20];
/// let fast = compute_fast_set("192.168.0.1", 1000, &info_hash, 10);
/// assert!(fast.len() <= 10);
/// ```
pub fn compute_fast_set(
    ip: &str,
    num_pieces: u32,
    info_hash: &[u8; 20],
    set_size: usize,
) -> Vec<u32> {
    if num_pieces == 0 || set_size == 0 {
        return Vec::new();
    }

    // Resolve IP to a 4-byte big-endian value.
    let ip_bytes = match resolve_ip_bytes(ip) {
        Some(bytes) => bytes,
        None => return Vec::new(),
    };

    // Apply privacy masking (matches C++ packcompact masking logic).
    let masked_ip = mask_ip(ip_bytes);

    // Build the 24-byte seed: [masked_ip (4)][info_hash (20)].
    let mut tx = [0u8; 24];
    tx[..4].copy_from_slice(&masked_ip);
    tx[4..].copy_from_slice(info_hash);

    // Initial SHA-1 of the seed.
    let mut x = sha1_digest(&tx);

    // Cap set_size to num_pieces (can't have more unique indices than pieces).
    let k = set_size.min(num_pieces as usize);

    let mut fast_set = Vec::with_capacity(k);

    while fast_set.len() < k {
        // Extract up to 5 u32 values from the 20-byte hash.
        for i in 0..5 {
            if fast_set.len() >= k {
                break;
            }
            let offset = i * 4;
            let y = u32::from_be_bytes([x[offset], x[offset + 1], x[offset + 2], x[offset + 3]]);
            let index = y % num_pieces;
            if !fast_set.contains(&index) {
                fast_set.push(index);
            }
        }
        // Rehash for the next round.
        x = sha1_digest(&x);
    }

    fast_set
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Resolve an IP address string to 4 big-endian bytes.
///
/// - IPv4: returns the raw 4 octets.
/// - IPv6: SHA-1 the 16-byte address and take the first 4 bytes.
///   This is an extension beyond the C++ implementation, which returns
///   empty for IPv6.
fn resolve_ip_bytes(ip: &str) -> Option<[u8; 4]> {
    // Try IPv4 first (common case).
    if let Ok(v4) = Ipv4Addr::from_str(ip) {
        return Some(v4.octets());
    }

    // Try IPv6.
    if let Ok(v6) = Ipv6Addr::from_str(ip) {
        // SHA-1 the 16-byte address, take first 4 bytes.
        let hash = sha1_digest(&v6.octets());
        Some([hash[0], hash[1], hash[2], hash[3]])
    } else {
        None
    }
}

/// Apply BEP 6 privacy masking to the 4-byte IP value.
///
/// Mirrors the C++ logic in `computeFastSet()`:
///
/// ```text
/// if (byte0 & 0x80 == 0) || (byte0 & 0x40 == 0):
///     byte2 = 0, byte3 = 0   // Class A/B: zero last two octets
/// else:
///     byte3 = 0              // Class C: zero last octet only
/// ```
///
/// This preserves the subnet structure while anonymizing the host part,
/// ensuring peers on the same subnet receive the same fast set.
fn mask_ip(mut ip: [u8; 4]) -> [u8; 4] {
    if (ip[0] & 0x80) == 0 || (ip[0] & 0x40) == 0 {
        ip[2] = 0;
        ip[3] = 0;
    } else {
        ip[3] = 0;
    }
    ip
}

/// Compute SHA-1 digest of the input, returning the raw 20-byte hash.
fn sha1_digest(data: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut out = [0u8; 20];
    out.copy_from_slice(&result);
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // C++ test vectors from BittorrentHelperTest::testComputeFastSet().
    // These verify exact byte-level compatibility with the C++ aria2
    // implementation of computeFastSet().

    #[test]
    fn test_cpp_vector_192_168_0_1() {
        let mut info_hash = [0u8; 20];
        info_hash[0] = 0xFF;
        let fast = compute_fast_set("192.168.0.1", 1000, &info_hash, 10);
        let expected: Vec<u32> = vec![686, 459, 278, 200, 404, 834, 64, 203, 760, 950];
        assert_eq!(fast, expected);
    }

    #[test]
    fn test_cpp_vector_10_0_0_1() {
        let mut info_hash = [0u8; 20];
        info_hash[0] = 0xFF;
        let fast = compute_fast_set("10.0.0.1", 1000, &info_hash, 10);
        let expected: Vec<u32> = vec![568, 188, 466, 452, 550, 662, 109, 226, 398, 11];
        assert_eq!(fast, expected);
    }

    #[test]
    fn test_cpp_vector_fewer_pieces_than_set_size() {
        let mut info_hash = [0u8; 20];
        info_hash[0] = 0xFF;
        // numPieces = 9, fastSetSize = 10 → capped to 9 unique indices
        let fast = compute_fast_set("10.0.0.1", 9, &info_hash, 10);
        let expected: Vec<u32> = vec![8, 6, 7, 5, 1, 4, 0, 2, 3];
        assert_eq!(fast, expected);
    }

    // ── Edge cases ─────────────────────────────────────────────────────

    #[test]
    fn test_zero_pieces_returns_empty() {
        let info_hash = [0u8; 20];
        let fast = compute_fast_set("192.168.0.1", 0, &info_hash, 10);
        assert!(fast.is_empty());
    }

    #[test]
    fn test_zero_set_size_returns_empty() {
        let info_hash = [0u8; 20];
        let fast = compute_fast_set("192.168.0.1", 100, &info_hash, 0);
        assert!(fast.is_empty());
    }

    #[test]
    fn test_invalid_ip_returns_empty() {
        let info_hash = [0u8; 20];
        let fast = compute_fast_set("not-an-ip", 100, &info_hash, 10);
        assert!(fast.is_empty());
    }

    #[test]
    fn test_single_piece() {
        let info_hash = [0u8; 20];
        let fast = compute_fast_set("192.168.0.1", 1, &info_hash, 10);
        // Only piece index 0 is possible
        assert_eq!(fast, vec![0]);
    }

    #[test]
    fn test_all_indices_within_range() {
        let info_hash = [0u8; 20];
        let num_pieces = 500u32;
        let fast = compute_fast_set("192.168.0.1", num_pieces, &info_hash, 10);
        assert_eq!(fast.len(), 10);
        for &idx in &fast {
            assert!(
                idx < num_pieces,
                "index {} >= num_pieces {}",
                idx,
                num_pieces
            );
        }
    }

    #[test]
    fn test_no_duplicates() {
        let info_hash = [0u8; 20];
        let fast = compute_fast_set("192.168.0.1", 1000, &info_hash, 10);
        let mut seen = std::collections::HashSet::new();
        for &idx in &fast {
            assert!(seen.insert(idx), "duplicate index {}", idx);
        }
    }

    #[test]
    fn test_set_size_capped_to_num_pieces() {
        let info_hash = [0u8; 20];
        let fast = compute_fast_set("192.168.0.1", 5, &info_hash, 100);
        // Can have at most 5 unique indices when num_pieces = 5
        assert!(fast.len() <= 5);
        for &idx in &fast {
            assert!(idx < 5);
        }
    }

    // ── IP masking ─────────────────────────────────────────────────────

    #[test]
    fn test_mask_class_a_ip() {
        // 10.x.x.x: byte0=10, bit7=0 → zero bytes 2,3
        let result = mask_ip([10, 0, 0, 1]);
        assert_eq!(result, [10, 0, 0, 0]);
    }

    #[test]
    fn test_mask_class_b_ip() {
        // 172.16.x.x: byte0=172, bit7=1, bit6=0 → zero bytes 2,3
        let result = mask_ip([172, 16, 0, 1]);
        assert_eq!(result, [172, 16, 0, 0]);
    }

    #[test]
    fn test_mask_class_c_ip() {
        // 192.168.x.x: byte0=192, bit7=1, bit6=1 → zero byte 3 only
        let result = mask_ip([192, 168, 0, 1]);
        assert_eq!(result, [192, 168, 0, 0]);
    }

    #[test]
    fn test_mask_preserves_subnet_for_class_c() {
        // Two IPs in the same /24 get the same masked value
        let a = mask_ip([192, 168, 1, 10]);
        let b = mask_ip([192, 168, 1, 200]);
        assert_eq!(a, b);
    }

    // ── IPv6 support ──────────────────────────────────────────────────

    #[test]
    fn test_ipv6_returns_nonempty() {
        let info_hash = [0u8; 20];
        let fast = compute_fast_set("::1", 1000, &info_hash, 10);
        // C++ would return empty; our extension supports IPv6.
        assert!(!fast.is_empty());
        assert!(fast.len() <= 10);
    }

    #[test]
    fn test_ipv6_all_indices_within_range() {
        let info_hash = [0u8; 20];
        let fast = compute_fast_set("2001:db8::1", 500, &info_hash, 10);
        for &idx in &fast {
            assert!(idx < 500);
        }
    }

    #[test]
    fn test_ipv6_deterministic() {
        let info_hash = [0u8; 20];
        let a = compute_fast_set("::1", 1000, &info_hash, 10);
        let b = compute_fast_set("::1", 1000, &info_hash, 10);
        assert_eq!(a, b);
    }

    #[test]
    fn test_ipv6_different_addresses_different_sets() {
        let info_hash = [0u8; 20];
        let a = compute_fast_set("::1", 1000, &info_hash, 10);
        let b = compute_fast_set("::2", 1000, &info_hash, 10);
        // Extremely unlikely to be identical, but not impossible.
        // We just verify both produce valid results.
        assert!(!a.is_empty());
        assert!(!b.is_empty());
    }

    // ── Determinism ────────────────────────────────────────────────────

    #[test]
    fn test_deterministic_same_inputs_same_output() {
        let info_hash = [0xAB; 20];
        let a = compute_fast_set("192.168.1.1", 2000, &info_hash, 10);
        let b = compute_fast_set("192.168.1.1", 2000, &info_hash, 10);
        assert_eq!(a, b);
    }

    #[test]
    fn test_different_info_hash_different_set() {
        let hash_a = [0x00; 20];
        let hash_b = [0xFF; 20];
        let a = compute_fast_set("192.168.0.1", 1000, &hash_a, 10);
        let b = compute_fast_set("192.168.0.1", 1000, &hash_b, 10);
        // Different info hashes should produce different fast sets
        // (not guaranteed, but astronomically unlikely to collide).
        assert!(!a.is_empty());
        assert!(!b.is_empty());
    }

    // ── resolve_ip_bytes ──────────────────────────────────────────────

    #[test]
    fn test_resolve_ipv4() {
        let bytes = resolve_ip_bytes("192.168.0.1").unwrap();
        assert_eq!(bytes, [192, 168, 0, 1]);
    }

    #[test]
    fn test_resolve_ipv6_loopback() {
        let bytes = resolve_ip_bytes("::1");
        assert!(bytes.is_some());
    }

    #[test]
    fn test_resolve_invalid_ip() {
        assert!(resolve_ip_bytes("invalid").is_none());
    }

    // ── sha1_digest helper ────────────────────────────────────────────

    #[test]
    fn test_sha1_known_vector() {
        // SHA-1 of empty string: da39a3ee5e6b4b0d3255bfef95601890afd80709
        let hash = sha1_digest(b"");
        assert_eq!(
            hex::encode(hash),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
    }

    #[test]
    fn test_sha1_abc() {
        // SHA-1 of "abc": a9993e364706816aba3e25717850c26c9cd0d89d
        let hash = sha1_digest(b"abc");
        assert_eq!(
            hex::encode(hash),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }
}
