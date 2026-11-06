use super::rpc_update::apply_rpc_option;
use crate::request::request_group::DownloadOptions;

#[test]
fn string_options_reach_their_typed_fields() {
    let cases = [
        ("bt-peer-blocklist", serde_json::json!("blocklist.txt")),
        ("peer-id-prefix", serde_json::json!("-AZ1234-")),
        ("peer-agent", serde_json::json!("aria2-test")),
        ("dht-listen-addr6", serde_json::json!("[::1]")),
        (
            "dht-entry-point-host",
            serde_json::json!("bootstrap.example"),
        ),
        ("dht-entry-point6", serde_json::json!("[2001:db8::1]:6881")),
        (
            "dht-entry-point-host6",
            serde_json::json!("bootstrap6.example"),
        ),
        ("dht-file-path6", serde_json::json!("dht6.dat")),
        ("dht-listen-addr", serde_json::json!("127.0.0.1")),
    ];

    for (key, value) in cases {
        let mut options = DownloadOptions::default();
        assert!(
            apply_rpc_option(&mut options, key, &value).unwrap(),
            "{key}"
        );
    }
}
