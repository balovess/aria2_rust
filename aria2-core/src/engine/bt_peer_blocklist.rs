#![allow(dead_code)]

//! IP range-based blocklist for BitTorrent peers.
//!
//! Supports CIDR notation (e.g. `192.168.1.0/24`, `::1/128`) and plain host
//! addresses. IPv4-mapped IPv6 addresses (e.g. `::ffff:192.168.1.1`) are
//! automatically converted to their IPv4 equivalent.
//!
//! Ranges are sorted and merged after each load, yielding O(log n) lookups via
//! binary search.

use std::io::{BufRead, BufReader, Read};
use std::net::IpAddr;

use tracing::debug;

// ---------------------------------------------------------------------------
// Range
// ---------------------------------------------------------------------------

/// A contiguous IP range stored as two 16-byte big-endian addresses.
///
/// For IPv4 ranges only the first 4 bytes are meaningful; the remaining 12 are
/// zero. For IPv6 ranges all 16 bytes are used.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Range {
    first: [u8; 16],
    last: [u8; 16],
}

// ---------------------------------------------------------------------------
// BtPeerBlocklist
// ---------------------------------------------------------------------------

/// Blocklist that checks whether a peer IP falls within any blocked CIDR range.
pub struct BtPeerBlocklist {
    ipv4_ranges: Vec<Range>,
    ipv6_ranges: Vec<Range>,
    rule_count: usize,
    revision: u64,
}

impl BtPeerBlocklist {
    /// Create an empty blocklist.
    pub fn new() -> Self {
        Self {
            ipv4_ranges: Vec::new(),
            ipv6_ranges: Vec::new(),
            rule_count: 0,
            revision: 1,
        }
    }

    /// Return `true` if *any* blocked range contains `ipaddr`.
    ///
    /// Invalid addresses simply return `false`.
    pub fn contains(&self, ipaddr: &str) -> bool {
        if self.rule_count == 0 {
            return false;
        }
        match parse_address(ipaddr) {
            Some(addr) => {
                let ranges = if addr.length == 4 {
                    &self.ipv4_ranges
                } else {
                    &self.ipv6_ranges
                };
                contains_address(ranges, &addr)
            }
            None => false,
        }
    }

    /// Parse a single CIDR rule (e.g. `10.0.0.0/8`) and add it to the
    /// blocklist.
    ///
    /// After insertion the ranges are re-sorted and re-merged so that
    /// [`contains`](Self::contains) remains correct.
    pub fn add_rule(&mut self, rule: &str) -> Result<(), String> {
        let (range, addr_len) = create_range(rule)?;
        if addr_len == 4 {
            self.ipv4_ranges.push(range);
            merge_ranges(&mut self.ipv4_ranges, 4);
        } else {
            self.ipv6_ranges.push(range);
            merge_ranges(&mut self.ipv6_ranges, 16);
        }
        self.rule_count += 1;
        self.revision += 1;
        Ok(())
    }

    /// Load blocklist rules from any `Read` source, one CIDR rule per line.
    ///
    /// Blank lines and lines starting with `#` are skipped.  All existing rules
    /// are replaced.
    pub fn load_from_reader(&mut self, reader: impl Read, source: &str) -> Result<(), String> {
        let mut ipv4: Vec<Range> = Vec::new();
        let mut ipv6: Vec<Range> = Vec::new();
        let mut rule_count: usize = 0;

        let buf_reader = BufReader::new(reader);
        for (line_num, line_result) in buf_reader.lines().enumerate() {
            let line = line_result
                .map_err(|e| format!("I/O error reading {}: {}", source, e))?;
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            match create_range(trimmed) {
                Ok((range, addr_len)) => {
                    if addr_len == 4 {
                        ipv4.push(range);
                    } else {
                        ipv6.push(range);
                    }
                    rule_count += 1;
                }
                Err(msg) => {
                    return Err(format!(
                        "Invalid BT peer blocklist rule at {}:{}: {}",
                        source,
                        line_num + 1,
                        msg
                    ));
                }
            }
        }

        merge_ranges(&mut ipv4, 4);
        merge_ranges(&mut ipv6, 16);

        self.ipv4_ranges = ipv4;
        self.ipv6_ranges = ipv6;
        self.rule_count = rule_count;
        self.revision += 1;

        debug!(
            rule_count = self.rule_count,
            source = source,
            "Loaded BT peer blocklist rules"
        );
        Ok(())
    }

    /// Remove all rules.
    pub fn clear(&mut self) {
        self.ipv4_ranges.clear();
        self.ipv6_ranges.clear();
        self.rule_count = 0;
        self.revision += 1;
    }

    /// Number of rules that were loaded (before merging).
    pub fn count(&self) -> usize {
        self.rule_count
    }

    /// Revision counter; incremented on every mutation.
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

impl Default for BtPeerBlocklist {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// ParsedAddress (internal)
// ---------------------------------------------------------------------------

/// Parsed IP address: 16-byte big-endian representation + effective byte length
/// (4 for IPv4, 16 for IPv6).
struct ParsedAddress {
    bytes: [u8; 16],
    length: usize,
}

// ---------------------------------------------------------------------------
// Address helpers
// ---------------------------------------------------------------------------

/// Convert an `IpAddr` to a `ParsedAddress`, normalising IPv4-mapped IPv6 to
/// plain IPv4.
fn parse_address(value: &str) -> Option<ParsedAddress> {
    let ip: IpAddr = value.parse().ok()?;
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            let mut bytes = [0u8; 16];
            bytes[..4].copy_from_slice(&octets);
            Some(ParsedAddress {
                bytes,
                length: 4,
            })
        }
        IpAddr::V6(v6) => {
            let octets = v6.octets();
            // Check for IPv4-mapped IPv6: ::ffff:x.x.x.x
            if octets[0..10] == [0u8; 10] && octets[10] == 0xff && octets[11] == 0xff {
                let mut bytes = [0u8; 16];
                bytes[..4].copy_from_slice(&octets[12..16]);
                Some(ParsedAddress {
                    bytes,
                    length: 4,
                })
            } else {
                let mut bytes = [0u8; 16];
                bytes.copy_from_slice(&octets);
                Some(ParsedAddress {
                    bytes,
                    length: 16,
                })
            }
        }
    }
}

/// Lexicographic comparison of the first `len` bytes.
fn less_address(lhs: &[u8; 16], rhs: &[u8; 16], len: usize) -> bool {
    lhs[..len] < rhs[..len]
}

/// `lhs <= rhs` over the first `len` bytes.
fn less_or_equal_address(lhs: &[u8; 16], rhs: &[u8; 16], len: usize) -> bool {
    !less_address(rhs, lhs, len)
}

// ---------------------------------------------------------------------------
// CIDR parsing
// ---------------------------------------------------------------------------

/// Parse a CIDR rule into a `Range`, returning the range and the effective
/// address byte length (4 or 16).
fn create_range(rule: &str) -> Result<(Range, usize), String> {
    let (addr_text, prefix_text) = match rule.find('/') {
        Some(slash) => (&rule[..slash], Some(&rule[slash + 1..])),
        None => (rule, None),
    };

    let addr = parse_address(addr_text).ok_or_else(|| format!("Invalid IP address: {}", addr_text))?;
    let addr_len = addr.length;

    let prefix_len: u32 = match prefix_text {
        Some(text) => text
            .parse::<u32>()
            .map_err(|_| format!("Invalid CIDR prefix length: {}", text))
            .and_then(|v| {
                if v > addr_len as u32 * 8 {
                    Err(format!(
                        "CIDR prefix length {} exceeds address bit width {}",
                        v,
                        addr_len as u32 * 8
                    ))
                } else {
                    Ok(v)
                }
            })?,
        None => addr_len as u32 * 8,
    };

    let mut range = Range {
        first: [0u8; 16],
        last: [0u8; 16],
    };

    for i in 0..addr_len {
        let remaining = prefix_len.saturating_sub(i as u32 * 8);
        let mask: u8 = if remaining >= 8 {
            0xff
        } else {
            0xffu8.checked_shl(8 - remaining).unwrap_or(0)
        };
        range.first[i] = addr.bytes[i] & mask;
        range.last[i] = addr.bytes[i] | (!mask);
    }

    Ok((range, addr_len))
}

// ---------------------------------------------------------------------------
// Range merging
// ---------------------------------------------------------------------------

/// Sort ranges by `first` address and merge overlapping/adjacent ranges
/// in-place.
fn merge_ranges(ranges: &mut Vec<Range>, len: usize) {
    ranges.sort_by(|a, b| {
        if less_address(&a.first, &b.first, len) {
            std::cmp::Ordering::Less
        } else if a.first[..len] == b.first[..len] {
            std::cmp::Ordering::Equal
        } else {
            std::cmp::Ordering::Greater
        }
    });

    if ranges.is_empty() {
        return;
    }

    let mut out = 0;
    let mut i = 1;
    while i < ranges.len() {
        if less_or_equal_address(&ranges[i].first, &ranges[out].last, len) {
            // Overlapping or adjacent — extend if the new range goes further.
            if less_address(&ranges[out].last, &ranges[i].last, len) {
                ranges[out].last = ranges[i].last;
            }
        } else {
            out += 1;
            ranges[out] = ranges[i].clone();
        }
        i += 1;
    }
    ranges.truncate(out + 1);
}

// ---------------------------------------------------------------------------
// Contains lookup
// ---------------------------------------------------------------------------

/// Binary-search check: return `true` if `addr` falls within any range.
///
/// After merging, ranges are non-overlapping and sorted by `first`. We find the
/// last range whose `first <= addr` (by locating the first range whose `first >
/// addr` and stepping back), then test `addr <= range.last`.
fn contains_address(ranges: &[Range], addr: &ParsedAddress) -> bool {
    // Find the first range whose `first` is strictly greater than addr.
    // partition_point requires the predicate to be true for a prefix then false,
    // so we use `addr >= range.first` (true for ranges with small first, false
    // for ranges with large first).
    let idx = ranges.partition_point(|r| !less_address(&addr.bytes, &r.first, addr.length));
    if idx == 0 {
        return false;
    }
    let prev = &ranges[idx - 1];
    less_or_equal_address(&prev.first, &addr.bytes, addr.length)
        && less_or_equal_address(&addr.bytes, &prev.last, addr.length)
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── Construction ──────────────────────────────────────────────────────

    #[test]
    fn empty_blocklist_contains_nothing() {
        let bl = BtPeerBlocklist::new();
        assert!(!bl.contains("10.0.0.1"));
        assert!(!bl.contains("::1"));
        assert_eq!(bl.count(), 0);
        assert_eq!(bl.revision(), 1);
    }

    // ── Single IPv4 CIDR rule ─────────────────────────────────────────────

    #[test]
    fn single_ipv4_cidr() {
        let mut bl = BtPeerBlocklist::new();
        bl.add_rule("10.0.0.0/8").unwrap();

        assert!(bl.contains("10.0.0.1"));
        assert!(bl.contains("10.255.255.255"));
        assert!(!bl.contains("9.255.255.255"));
        assert!(!bl.contains("11.0.0.0"));
        assert_eq!(bl.count(), 1);
    }

    #[test]
    fn single_ipv4_slash_24() {
        let mut bl = BtPeerBlocklist::new();
        bl.add_rule("192.168.1.0/24").unwrap();

        assert!(bl.contains("192.168.1.0"));
        assert!(bl.contains("192.168.1.255"));
        assert!(!bl.contains("192.168.0.255"));
        assert!(!bl.contains("192.168.2.0"));
    }

    // ── Single IPv6 CIDR rule ─────────────────────────────────────────────

    #[test]
    fn single_ipv6_cidr() {
        let mut bl = BtPeerBlocklist::new();
        bl.add_rule("2001:db8::/32").unwrap();

        assert!(bl.contains("2001:db8::1"));
        assert!(bl.contains("2001:db8:ffff:ffff:ffff:ffff:ffff:ffff"));
        assert!(!bl.contains("2001:db9::"));
        assert!(!bl.contains("2001:db7:ffff:ffff:ffff:ffff:ffff:ffff"));
    }

    // ── Host address (no /prefix) ─────────────────────────────────────────

    #[test]
    fn host_address_ipv4() {
        let mut bl = BtPeerBlocklist::new();
        bl.add_rule("192.168.1.100").unwrap();

        assert!(bl.contains("192.168.1.100"));
        assert!(!bl.contains("192.168.1.101"));
        assert!(!bl.contains("192.168.1.99"));
    }

    #[test]
    fn host_address_ipv6() {
        let mut bl = BtPeerBlocklist::new();
        bl.add_rule("::1").unwrap();

        assert!(bl.contains("::1"));
        assert!(!bl.contains("::2"));
    }

    // ── Overlapping range merging ─────────────────────────────────────────

    #[test]
    fn overlapping_ipv4_ranges_merge() {
        let mut bl = BtPeerBlocklist::new();
        // 10.0.0.0/8  => 10.0.0.0 – 10.255.255.255
        bl.add_rule("10.0.0.0/8").unwrap();
        // 10.1.0.0/16 => 10.1.0.0 – 10.1.255.255  (subsumed by /8)
        bl.add_rule("10.1.0.0/16").unwrap();

        // After merge there should be only 1 range in ipv4_ranges.
        assert_eq!(bl.ipv4_ranges.len(), 1);
        assert_eq!(bl.count(), 2);
    }

    #[test]
    fn overlapping_adjacent_ipv4_ranges_merge() {
        let mut bl = BtPeerBlocklist::new();
        // /23 covers 192.168.0.0 – 192.168.1.255
        bl.add_rule("192.168.0.0/23").unwrap();
        // /24 covers 192.168.1.0 – 192.168.1.255 (overlaps with /23)
        bl.add_rule("192.168.1.0/24").unwrap();

        // After merge there should be only 1 range.
        assert_eq!(bl.ipv4_ranges.len(), 1);
        assert!(bl.contains("192.168.0.0"));
        assert!(bl.contains("192.168.1.255"));
        assert!(!bl.contains("192.168.2.0"));
    }

    #[test]
    fn overlapping_ipv6_ranges_merge() {
        let mut bl = BtPeerBlocklist::new();
        bl.add_rule("2001:db8::/32").unwrap();
        bl.add_rule("2001:db8:1::/48").unwrap();

        assert_eq!(bl.ipv6_ranges.len(), 1);
    }

    // ── Revision increment ────────────────────────────────────────────────

    #[test]
    fn revision_increments_on_add_and_clear() {
        let mut bl = BtPeerBlocklist::new();
        let rev0 = bl.revision();

        bl.add_rule("10.0.0.0/8").unwrap();
        assert_eq!(bl.revision(), rev0 + 1);

        bl.add_rule("172.16.0.0/12").unwrap();
        assert_eq!(bl.revision(), rev0 + 2);

        bl.clear();
        assert_eq!(bl.revision(), rev0 + 3);
    }

    // ── Clear ─────────────────────────────────────────────────────────────

    #[test]
    fn clear_empties_all_rules() {
        let mut bl = BtPeerBlocklist::new();
        bl.add_rule("10.0.0.0/8").unwrap();
        bl.add_rule("::1/128").unwrap();
        assert_eq!(bl.count(), 2);

        bl.clear();
        assert_eq!(bl.count(), 0);
        assert!(!bl.contains("10.0.0.1"));
        assert!(!bl.contains("::1"));
    }

    // ── Invalid input handling ────────────────────────────────────────────

    #[test]
    fn invalid_ip_address_rejected() {
        let mut bl = BtPeerBlocklist::new();
        assert!(bl.add_rule("not-an-ip").is_err());
    }

    #[test]
    fn invalid_prefix_length_rejected() {
        let mut bl = BtPeerBlocklist::new();
        assert!(bl.add_rule("10.0.0.0/33").is_err());
        assert!(bl.add_rule("::1/129").is_err());
    }

    #[test]
    fn contains_with_invalid_ip_returns_false() {
        let mut bl = BtPeerBlocklist::new();
        bl.add_rule("10.0.0.0/8").unwrap();
        assert!(!bl.contains("garbage"));
    }

    // ── load_from_reader ──────────────────────────────────────────────────

    #[test]
    fn load_from_reader_multiline() {
        let input = "\
# This is a comment
10.0.0.0/8
172.16.0.0/12

192.168.0.0/16
";
        let mut bl = BtPeerBlocklist::new();
        bl.load_from_reader(input.as_bytes(), "test-input").unwrap();

        assert_eq!(bl.count(), 3);
        assert!(bl.contains("10.1.2.3"));
        assert!(bl.contains("172.16.0.1"));
        assert!(bl.contains("192.168.100.200"));
        assert!(!bl.contains("8.8.8.8"));
    }

    #[test]
    fn load_from_reader_invalid_line_errors() {
        let input = "10.0.0.0/8\nnot-an-ip\n";
        let mut bl = BtPeerBlocklist::new();
        let result = bl.load_from_reader(input.as_bytes(), "bad-input");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("bad-input:2"), "Error should mention line 2: {}", err);
    }

    // ── IPv4-mapped IPv6 ──────────────────────────────────────────────────

    #[test]
    fn ipv4_mapped_ipv6_treated_as_ipv4() {
        let mut bl = BtPeerBlocklist::new();
        // Block 192.168.1.0/24 as IPv4
        bl.add_rule("192.168.1.0/24").unwrap();

        // ::ffff:192.168.1.5 should be treated as 192.168.1.5 and matched
        assert!(bl.contains("::ffff:192.168.1.5"));
        assert!(!bl.contains("::ffff:10.0.0.1"));
    }

    #[test]
    fn ipv4_mapped_ipv6_rule() {
        let mut bl = BtPeerBlocklist::new();
        // ::ffff:10.0.0.0 is converted to IPv4 10.0.0.0 (length=4).
        // With /8 prefix: blocks 10.0.0.0 – 10.255.255.255.
        bl.add_rule("::ffff:10.0.0.0/8").unwrap();

        assert!(bl.contains("10.0.0.1"));
        assert!(bl.contains("10.255.255.255"));
        assert!(!bl.contains("11.0.0.0"));
    }

    #[test]
    fn ipv4_mapped_ipv6_prefix_too_large_rejected() {
        let mut bl = BtPeerBlocklist::new();
        // ::ffff:10.0.0.0 becomes IPv4 (32-bit), so /104 is invalid.
        assert!(bl.add_rule("::ffff:10.0.0.0/104").is_err());
    }

    // ── Edge cases ────────────────────────────────────────────────────────

    #[test]
    fn prefix_length_zero_matches_all() {
        let mut bl = BtPeerBlocklist::new();
        bl.add_rule("0.0.0.0/0").unwrap();

        assert!(bl.contains("1.2.3.4"));
        assert!(bl.contains("255.255.255.255"));
    }

    #[test]
    fn prefix_length_128_matches_exact_ipv6() {
        let mut bl = BtPeerBlocklist::new();
        bl.add_rule("fe80::1/128").unwrap();

        assert!(bl.contains("fe80::1"));
        assert!(!bl.contains("fe80::2"));
    }

    #[test]
    fn mixed_ipv4_and_ipv6_rules() {
        let mut bl = BtPeerBlocklist::new();
        bl.add_rule("10.0.0.0/8").unwrap();
        bl.add_rule("fc00::/7").unwrap();

        assert!(bl.contains("10.1.2.3"));
        assert!(bl.contains("fc00::1"));
        assert!(!bl.contains("192.168.1.1"));
        assert!(!bl.contains("2001:db8::1"));
    }

    // ── Default trait ─────────────────────────────────────────────────────

    #[test]
    fn default_equals_new() {
        let n = BtPeerBlocklist::new();
        let d = BtPeerBlocklist::default();
        assert_eq!(n.count(), d.count());
        assert_eq!(n.revision(), d.revision());
    }
}
