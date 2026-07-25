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
    bl.add_rule("10.0.0.0/8").unwrap();
    bl.add_rule("10.1.0.0/16").unwrap();

    assert_eq!(bl.ipv4_ranges.len(), 1);
    assert_eq!(bl.count(), 2);
}

#[test]
fn overlapping_adjacent_ipv4_ranges_merge() {
    let mut bl = BtPeerBlocklist::new();
    bl.add_rule("192.168.0.0/23").unwrap();
    bl.add_rule("192.168.1.0/24").unwrap();

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
    assert!(
        err.contains("bad-input:2"),
        "Error should mention line 2: {}",
        err
    );
}

// ── load_from_file ────────────────────────────────────────────────────

#[test]
fn load_from_file_reads_existing_file() {
    let dir = std::env::temp_dir().join("aria2_rust_blocklist_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("blocklist.txt");

    let content = "\
# private ranges
10.0.0.0/8
172.16.0.0/12
192.168.0.0/16
";
    std::fs::write(&path, content).unwrap();

    let mut bl = BtPeerBlocklist::new();
    bl.load_from_file(&path).unwrap();

    assert_eq!(bl.count(), 3);
    assert!(bl.contains("10.1.2.3"));
    assert!(bl.contains("172.16.0.1"));
    assert!(bl.contains("192.168.100.200"));
    assert!(!bl.contains("8.8.8.8"));

    // Cleanup
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
}

#[test]
fn load_from_file_missing_file_errors() {
    let path = std::path::PathBuf::from("/nonexistent/blocklist.txt");
    let mut bl = BtPeerBlocklist::new();
    let result = bl.load_from_file(&path);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Cannot open"));
}

// ── IPv4-mapped IPv6 ──────────────────────────────────────────────────

#[test]
fn ipv4_mapped_ipv6_treated_as_ipv4() {
    let mut bl = BtPeerBlocklist::new();
    bl.add_rule("192.168.1.0/24").unwrap();

    assert!(bl.contains("::ffff:192.168.1.5"));
    assert!(!bl.contains("::ffff:10.0.0.1"));
}

#[test]
fn ipv4_mapped_ipv6_rule() {
    let mut bl = BtPeerBlocklist::new();
    bl.add_rule("::ffff:10.0.0.0/8").unwrap();

    assert!(bl.contains("10.0.0.1"));
    assert!(bl.contains("10.255.255.255"));
    assert!(!bl.contains("11.0.0.0"));
}

#[test]
fn ipv4_mapped_ipv6_prefix_too_large_rejected() {
    let mut bl = BtPeerBlocklist::new();
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
