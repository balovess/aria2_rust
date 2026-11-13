use super::*;

#[tokio::test]
async fn policy_udp_client_binds_the_requested_dual_stack_family() {
    let policy = OutboundNetworkPolicy::new(vec![
        "127.0.0.2".parse().expect("parse IPv4 source"),
        "::1".parse().expect("parse IPv6 source"),
    ])
    .expect("dual-stack policy should build");

    let ipv4 = UdpTrackerClient::new_with_policy_for_family(0, &policy, false)
        .await
        .expect("IPv4 UDP tracker client should bind");
    assert_eq!(
        ipv4.local_addr().unwrap().ip(),
        "127.0.0.2".parse::<std::net::IpAddr>().unwrap()
    );

    let ipv6 = UdpTrackerClient::new_with_policy_for_family(0, &policy, true)
        .await
        .expect("IPv6 UDP tracker client should bind");
    assert_eq!(
        ipv6.local_addr().unwrap().ip(),
        "::1".parse::<std::net::IpAddr>().unwrap()
    );
}

#[tokio::test]
async fn udp_tracker_url_resolution_preserves_explicit_and_default_ports() {
    let policy = OutboundNetworkPolicy::direct();

    let default_port = resolve_udp_tracker_addr("udp://127.0.0.1/announce", &policy)
        .await
        .expect("UDP tracker without a port should use the protocol default");
    assert_eq!(default_port, "127.0.0.1:6881".parse().unwrap());

    let explicit_port = resolve_udp_tracker_addr("udp://127.0.0.1:6969/announce", &policy)
        .await
        .expect("explicit UDP tracker port should be preserved");
    assert_eq!(explicit_port, "127.0.0.1:6969".parse().unwrap());
}

#[tokio::test]
async fn udp_tracker_url_resolution_preserves_invalid_url_errors() {
    let policy = OutboundNetworkPolicy::direct();

    assert_eq!(
        resolve_udp_tracker_addr("https://tracker.example/announce", &policy)
            .await
            .unwrap_err(),
        "invalid UDP tracker URL: https://tracker.example/announce"
    );
    assert_eq!(
        resolve_udp_tracker_addr("udp:///announce", &policy)
            .await
            .unwrap_err(),
        "missing UDP tracker host: udp:///announce"
    );
}
