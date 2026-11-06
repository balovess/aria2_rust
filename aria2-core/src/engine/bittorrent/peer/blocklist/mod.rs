//! IP range-based blocklist for BitTorrent peers.
//!
//! Supports CIDR notation (e.g. `192.168.1.0/24`, `::1/128`) and plain host
//! addresses. IPv4-mapped IPv6 addresses (e.g. `::ffff:192.168.1.1`) are
//! automatically converted to their IPv4 equivalent.
//!
//! Ranges are sorted and merged after each load, yielding O(log n) lookups via
//! binary search.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::net::IpAddr;
use std::path::Path;

use tracing::debug;

#[cfg(test)]
mod tests;

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
    /// Blank lines and lines starting with `#` are skipped. All existing rules
    /// are replaced.
    pub fn load_from_reader(&mut self, reader: impl Read, source: &str) -> Result<(), String> {
        let mut ipv4: Vec<Range> = Vec::new();
        let mut ipv6: Vec<Range> = Vec::new();
        let mut rule_count: usize = 0;

        let buf_reader = BufReader::new(reader);
        for (line_num, line_result) in buf_reader.lines().enumerate() {
            let line = line_result.map_err(|e| format!("I/O error reading {}: {}", source, e))?;
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

    /// Load blocklist rules from a file, one CIDR rule per line.
    ///
    /// Equivalent to C++ `BtPeerBlocklist::load(const std::string& path)`.
    pub fn load_from_file(&mut self, path: &Path) -> Result<(), String> {
        let file = File::open(path)
            .map_err(|e| format!("Cannot open BT peer blocklist: {}: {}", path.display(), e))?;
        self.load_from_reader(file, &path.display().to_string())
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
            Some(ParsedAddress { bytes, length: 4 })
        }
        IpAddr::V6(v6) => {
            let octets = v6.octets();
            // Check for IPv4-mapped IPv6: ::ffff:x.x.x.x
            if octets[0..10] == [0u8; 10] && octets[10] == 0xff && octets[11] == 0xff {
                let mut bytes = [0u8; 16];
                bytes[..4].copy_from_slice(&octets[12..16]);
                Some(ParsedAddress { bytes, length: 4 })
            } else {
                let mut bytes = [0u8; 16];
                bytes.copy_from_slice(&octets);
                Some(ParsedAddress { bytes, length: 16 })
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

    let addr =
        parse_address(addr_text).ok_or_else(|| format!("Invalid IP address: {}", addr_text))?;
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
            // Overlapping or adjacent -- extend if the new range goes further.
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
    let idx = ranges.partition_point(|r| !less_address(&addr.bytes, &r.first, addr.length));
    if idx == 0 {
        return false;
    }
    let prev = &ranges[idx - 1];
    less_or_equal_address(&prev.first, &addr.bytes, addr.length)
        && less_or_equal_address(&addr.bytes, &prev.last, addr.length)
}
