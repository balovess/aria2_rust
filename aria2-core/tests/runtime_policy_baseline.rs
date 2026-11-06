#![cfg(feature = "bittorrent")]

//! Public runtime policy regression checks against the project-owned
//! compatibility baseline.

use std::collections::BTreeSet;

use aria2_core::config::{
    INITIAL_REQUEST_OPTIONS, RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS, RUNTIME_CHANGEABLE_OPTIONS,
    RUNTIME_GLOBAL_CHANGEABLE_OPTIONS,
};

const COMPATIBILITY_POLICIES: &str = include_str!("fixtures/compatibility_option_policies.txt");

// Rust keeps the original RPC policy as the baseline and explicitly registers
// implemented Rust-only extensions here rather than changing the upstream
// compatibility fixture.
const RUST_HTTP2_POLICY_EXTENSIONS: &[&str] = &[
    "http-version",
    "max-http2-sessions-per-server",
    "max-http2-streams-per-session",
];

const RUST_HTTP_RANGE_POLICY_EXTENSIONS: &[&str] = &["min-http-range-size"];

const RUST_BITTORRENT_POLICY_EXTENSIONS: &[&str] = &[
    "bt-max-upload-slots",
    "bt-optimistic-unchoke-interval",
    "bt-snubbed-timeout",
];

const RUST_DHT_POLICY_EXTENSIONS: &[&str] = &[
    "enable-dht",
    "enable-dht6",
    "dht-listen-port",
    "dht-listen-addr",
    "dht-listen-addr6",
    "dht-entry-point",
    "dht-entry-point-host",
    "dht-entry-point-port",
    "dht-entry-point6",
    "dht-entry-point-host6",
    "dht-entry-point-port6",
    "dht-file-path",
    "dht-file-path6",
    "dht-message-timeout",
    "dht-refresh-check-interval",
    "dht-token-rotation-interval",
    "dht-node-contact-interval",
    "dht-cleanup-interval",
    "dht-save-interval",
    "dht-bootstrap-timeout",
    "dht-max-concurrent-lookups",
    "dht-persistence-max-age",
];

fn compatibility_policy_names(policy: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut selected = false;

    for line in COMPATIBILITY_POLICIES.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            selected = section == policy;
            continue;
        }
        if selected {
            assert!(
                names.insert(line.to_owned()),
                "{policy} repeats option {line}"
            );
        }
    }

    assert!(
        !names.is_empty(),
        "compatibility baseline has no {policy} entries"
    );
    names
}

fn policy_set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn assert_policy_matches_baseline(
    policy: &str,
    baseline: BTreeSet<String>,
    rust: &[&str],
    extensions: &[&str],
) {
    let mut expected = baseline;
    for extension in extensions {
        assert!(
            expected.insert((*extension).to_owned()),
            "{policy} extension {extension} is already in the compatibility baseline"
        );
    }
    let actual = policy_set(rust);
    let missing = expected.difference(&actual).collect::<Vec<_>>();
    let extra = actual.difference(&expected).collect::<Vec<_>>();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "{policy} policy differs from compatibility baseline; missing={missing:?}, extra={extra:?}"
    );
}

#[test]
fn runtime_policies_match_compatibility_baseline_with_explicit_rust_extensions() {
    let mut initial_extensions = vec!["bt-tracker-stopped-timeout"];
    initial_extensions.extend_from_slice(RUST_BITTORRENT_POLICY_EXTENSIONS);
    initial_extensions.extend_from_slice(RUST_DHT_POLICY_EXTENSIONS);
    initial_extensions.extend_from_slice(RUST_HTTP2_POLICY_EXTENSIONS);
    initial_extensions.extend_from_slice(RUST_HTTP_RANGE_POLICY_EXTENSIONS);
    assert_policy_matches_baseline(
        "setInitialOption",
        compatibility_policy_names("setInitialOption"),
        INITIAL_REQUEST_OPTIONS,
        &initial_extensions,
    );
    let mut global_extensions = vec![
        "bt-tracker-source",
        "bt-tracker-update-interval",
        "bt-tracker-stopped-timeout",
        "enable-public-trackers",
    ];
    global_extensions.extend_from_slice(RUST_DHT_POLICY_EXTENSIONS);
    global_extensions.extend_from_slice(RUST_HTTP2_POLICY_EXTENSIONS);
    global_extensions.extend_from_slice(RUST_HTTP_RANGE_POLICY_EXTENSIONS);
    assert_policy_matches_baseline(
        "setChangeGlobalOption",
        compatibility_policy_names("setChangeGlobalOption"),
        RUNTIME_GLOBAL_CHANGEABLE_OPTIONS,
        &global_extensions,
    );
    let mut reserved_extensions = vec!["enable-public-trackers"];
    reserved_extensions.extend_from_slice(RUST_BITTORRENT_POLICY_EXTENSIONS);
    reserved_extensions.extend_from_slice(RUST_DHT_POLICY_EXTENSIONS);
    reserved_extensions.extend_from_slice(RUST_HTTP2_POLICY_EXTENSIONS);
    reserved_extensions.extend_from_slice(RUST_HTTP_RANGE_POLICY_EXTENSIONS);
    assert_policy_matches_baseline(
        "setChangeOptionForReserved",
        compatibility_policy_names("setChangeOptionForReserved"),
        RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS,
        &reserved_extensions,
    );
    assert_policy_matches_baseline(
        "setChangeOption",
        compatibility_policy_names("setChangeOption"),
        RUNTIME_CHANGEABLE_OPTIONS,
        &[],
    );
}
