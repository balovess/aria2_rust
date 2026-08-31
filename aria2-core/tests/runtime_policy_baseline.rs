#![cfg(feature = "bittorrent")]

//! Public runtime policy regression checks against the project-owned
//! compatibility baseline.

use std::collections::BTreeSet;

use aria2_core::config::{
    INITIAL_REQUEST_OPTIONS, RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS, RUNTIME_CHANGEABLE_OPTIONS,
    RUNTIME_GLOBAL_CHANGEABLE_OPTIONS,
};

const COMPATIBILITY_POLICIES: &str = include_str!("fixtures/compatibility_option_policies.txt");

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
    assert_policy_matches_baseline(
        "setInitialOption",
        compatibility_policy_names("setInitialOption"),
        INITIAL_REQUEST_OPTIONS,
        &["bt-tracker-stopped-timeout"],
    );
    assert_policy_matches_baseline(
        "setChangeGlobalOption",
        compatibility_policy_names("setChangeGlobalOption"),
        RUNTIME_GLOBAL_CHANGEABLE_OPTIONS,
        &[
            "bt-tracker-source",
            "bt-tracker-update-interval",
            "bt-tracker-stopped-timeout",
            "enable-public-trackers",
        ],
    );
    assert_policy_matches_baseline(
        "setChangeOptionForReserved",
        compatibility_policy_names("setChangeOptionForReserved"),
        RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS,
        &["enable-public-trackers"],
    );
    assert_policy_matches_baseline(
        "setChangeOption",
        compatibility_policy_names("setChangeOption"),
        RUNTIME_CHANGEABLE_OPTIONS,
        &[],
    );
}
