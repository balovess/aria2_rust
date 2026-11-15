use super::*;

#[tokio::test]
async fn ipv6_dht_options_enter_the_protocol_engine_config() {
    let options = DownloadOptions {
        enable_dht6: true,
        dht_listen_port: Some("49001-49002".to_string()),
        dht_listen_addr6: Some("::1".to_string()),
        dht_entry_point6: Some("[::1]:49003".to_string()),
        dht_file_path6: Some("dht6.dat".to_string()),
        dht_message_timeout: 7,
        ..Default::default()
    };

    let config = build_dht_engine_config(&options)
        .await
        .expect("IPv6 DHT options should build a protocol config");

    assert_eq!(config.listen_addr, Some("::1".parse().unwrap()));
    assert_eq!(config.port, 49001);
    assert_eq!(config.port_range, Some(vec![49001, 49002]));
    assert_eq!(
        config.bootstrap_nodes,
        vec!["[::1]:49003".parse::<SocketAddr>().unwrap()]
    );
    assert_eq!(
        config.dht_file_path.as_deref(),
        Some(std::path::Path::new("dht6.dat"))
    );
    assert_eq!(config.query_timeout, Duration::from_secs(7));
}

#[tokio::test]
async fn process_dht_tuning_options_enter_the_protocol_engine_config() {
    let options = DownloadOptions {
        dht_message_timeout: 11,
        dht_refresh_check_interval: 301,
        dht_token_rotation_interval: 601,
        dht_node_contact_interval: 901,
        dht_cleanup_interval: 302,
        dht_save_interval: 1801,
        dht_bootstrap_timeout: 61,
        dht_max_concurrent_lookups: 23,
        dht_persistence_max_age: 86_401,
        ..Default::default()
    };

    let config = build_dht_engine_config(&options)
        .await
        .expect("DHT tuning options should build a protocol config");

    assert_eq!(config.query_timeout, Duration::from_secs(11));
    assert_eq!(config.refresh_check_interval, Duration::from_secs(301));
    assert_eq!(config.token_rotation_interval, Duration::from_secs(601));
    assert_eq!(config.node_contact_interval, Duration::from_secs(901));
    assert_eq!(config.cleanup_interval, Duration::from_secs(302));
    assert_eq!(config.save_interval, Duration::from_secs(1801));
    assert_eq!(config.bootstrap_timeout, Duration::from_secs(61));
    assert_eq!(config.max_concurrent_lookups, 23);
    assert_eq!(config.persistence_max_age, Duration::from_secs(86_401));
    let mut dht_options = DhtEngineOptions::from(&options);
    dht_options.dht_max_peer_info_hashes = 1200;
    dht_options.dht_max_peers_per_info_hash = 37;
    dht_options.dht_max_items = 512;
    let configured = build_dht_engine_config_for_family(
        &dht_options,
        &crate::network::OutboundNetworkPolicy::direct(),
        false,
    )
    .await
    .expect("DHT options should map into protocol config")
    .expect("IPv4 DHT config should exist");
    assert_eq!(configured.max_stored_items, 512);
    assert_eq!(
        dht_options.protocol_peer_storage_config(),
        aria2_protocol::bittorrent::dht::DhtPeerStorageConfig {
            max_info_hashes: 1200,
            max_peers_per_info_hash: 37,
        }
    );
}

#[tokio::test]
async fn dual_stack_policy_selects_matching_dht_udp_source_without_explicit_listen_addr() {
    let options6 = DownloadOptions {
        enable_dht6: true,
        dht_entry_point6: Some("[::1]:49003".to_string()),
        ..Default::default()
    };
    let policy = crate::network::OutboundNetworkPolicy::new(vec![
        "127.0.0.2".parse().expect("parse IPv4 source"),
        "::1".parse().expect("parse IPv6 source"),
    ])
    .expect("dual-stack policy should build");

    let config6 = build_dht_engine_config_with_policy(&options6, &policy)
        .await
        .expect("DHT6 config should select a compatible policy source");

    assert_eq!(config6.listen_addr, Some("::1".parse().unwrap()));

    let options4 = DownloadOptions {
        dht_entry_point: Some(vec!["127.0.0.1:49004".to_string()]),
        ..Default::default()
    };
    let config4 = build_dht_engine_config_with_policy(&options4, &policy)
        .await
        .expect("DHT4 config should select a compatible policy source");

    assert_eq!(config4.listen_addr, Some("127.0.0.2".parse().unwrap()));
}

#[tokio::test]
async fn dual_stack_options_build_independent_family_configs_and_snapshots() {
    let options = DownloadOptions {
        enable_dht: true,
        enable_dht6: true,
        dht_listen_addr: Some("127.0.0.1".to_string()),
        dht_listen_addr6: Some("::1".to_string()),
        dht_file_path: Some("v4.dat".to_string()),
        dht_file_path6: Some("v6.dat".to_string()),
        ..Default::default()
    };
    let policy = crate::network::OutboundNetworkPolicy::direct();

    let dht_options = DhtEngineOptions::from(&options);
    let config4 = build_dht_engine_config_for_family(&dht_options, &policy, false)
        .await
        .expect("IPv4 DHT options should build independently")
        .expect("direct IPv4 DHT should be available");
    let config6 = build_dht_engine_config_for_family(&dht_options, &policy, true)
        .await
        .expect("IPv6 DHT options should build independently")
        .expect("direct IPv6 DHT should be available");

    assert_eq!(config4.listen_addr, Some("127.0.0.1".parse().unwrap()));
    assert_eq!(config6.listen_addr, Some("::1".parse().unwrap()));
    assert_eq!(config4.dht_file_path.as_deref(), Some(Path::new("v4.dat")));
    assert_eq!(config6.dht_file_path.as_deref(), Some(Path::new("v6.dat")));
}

#[tokio::test]
async fn constrained_policy_skips_dht_for_an_unavailable_address_family() {
    let options = DownloadOptions {
        enable_dht6: true,
        ..Default::default()
    };
    let policy = crate::network::OutboundNetworkPolicy::single("::1".parse().unwrap());

    let dht_options = DhtEngineOptions::from(&options);
    let config4 = build_dht_engine_config_for_family(&dht_options, &policy, false)
        .await
        .expect("an unavailable address family should be skipped");
    let config6 = build_dht_engine_config_for_family(&dht_options, &policy, true)
        .await
        .expect("the configured address family should build");

    assert!(config4.is_none());
    assert_eq!(config6.unwrap().listen_addr, Some("::1".parse().unwrap()));
}

#[tokio::test]
async fn constrained_policy_rejects_a_conflicting_explicit_dht_address() {
    let options = DownloadOptions {
        dht_listen_addr: Some("127.0.0.2".to_string()),
        ..Default::default()
    };
    let policy = crate::network::OutboundNetworkPolicy::single("127.0.0.1".parse().unwrap());

    let dht_options = DhtEngineOptions::from(&options);
    let error = build_dht_engine_config_for_family(&dht_options, &policy, false)
        .await
        .expect_err("DHT must not bind outside the selected source address");

    assert!(
        matches!(error, Aria2Error::Fatal(FatalError::Config(message)) if message.contains("not an outbound source address"))
    );
}

#[tokio::test]
async fn explicit_dns_failure_disables_default_dht_bootstrap_without_system_fallback() {
    let options = DownloadOptions::default();
    let dht_options = DhtEngineOptions::from(&options);
    let policy = crate::network::OutboundNetworkPolicy::direct();
    let dns_cache = tokio::sync::Mutex::new(DnsCache::with_dns_servers(
        Duration::from_millis(200),
        vec!["127.0.0.1:9".parse().unwrap()],
    ));

    let config =
        build_dht_engine_config_for_family_with_dns_cache(&dht_options, &policy, &dns_cache, false)
            .await
            .expect("unavailable default bootstrap servers must not fail the download")
            .expect("direct DHT should remain available");

    assert!(config.bootstrap_nodes.is_empty());
    assert!(!config.bootstrap_on_start);
}

#[tokio::test]
async fn explicit_dns_resolves_default_dht_bootstrap_endpoints() {
    use tokio::net::UdpSocket;

    let socket = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("DNS fixture should bind");
    let server_address = socket.local_addr().expect("DNS fixture address");
    let dns_server = tokio::spawn(async move {
        let mut request = [0_u8; 512];
        loop {
            let Ok((length, peer)) = socket.recv_from(&mut request).await else {
                break;
            };
            let request = &request[..length];
            let mut question_end = 12;
            while question_end < request.len() && request[question_end] != 0 {
                question_end += 1;
            }
            question_end += 5;
            if question_end > request.len() {
                continue;
            }
            let query_type =
                u16::from_be_bytes([request[question_end - 4], request[question_end - 3]]);
            let has_ipv4_answer = query_type == 1;
            let mut response = Vec::with_capacity(question_end + 16);
            response.extend_from_slice(&request[..2]);
            response.extend_from_slice(&[
                0x81,
                0x80,
                0,
                1,
                0,
                u8::from(has_ipv4_answer),
                0,
                0,
                0,
                0,
            ]);
            response.extend_from_slice(&request[12..question_end]);
            if has_ipv4_answer {
                response
                    .extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 127, 0, 0, 1]);
            }
            let _ = socket.send_to(&response, peer).await;
        }
    });

    let options = DownloadOptions::default();
    let dht_options = DhtEngineOptions::from(&options);
    let policy = crate::network::OutboundNetworkPolicy::direct();
    let dns_cache = tokio::sync::Mutex::new(DnsCache::with_dns_servers(
        Duration::from_secs(2),
        vec![server_address],
    ));

    let config =
        build_dht_engine_config_for_family_with_dns_cache(&dht_options, &policy, &dns_cache, false)
            .await
            .expect("DHT config should resolve through its explicit DNS server")
            .expect("direct DHT should be available");
    dns_server.abort();

    assert!(config.bootstrap_on_start);
    assert!(!config.bootstrap_nodes.is_empty());
    assert!(
        config
            .bootstrap_nodes
            .iter()
            .all(|address| address.ip().is_loopback() && address.port() == 6881)
    );
}

#[tokio::test]
async fn async_dns_disabled_uses_system_resolution_for_custom_bootstrap() {
    let options = DownloadOptions {
        async_dns: false,
        dht_entry_point: Some(vec!["localhost:49003".to_string()]),
        ..Default::default()
    };
    let dht_options = DhtEngineOptions::from(&options);
    let policy = crate::network::OutboundNetworkPolicy::direct();
    let dns_cache = tokio::sync::Mutex::new(DnsCache::with_dns_servers(
        Duration::from_millis(200),
        vec!["127.0.0.1:9".parse().unwrap()],
    ));

    let config =
        build_dht_engine_config_for_family_with_dns_cache(&dht_options, &policy, &dns_cache, false)
            .await
            .expect("system resolver should resolve localhost when async DNS is disabled")
            .expect("direct DHT should be available");

    assert_eq!(config.bootstrap_nodes.len(), 1);
    assert!(config.bootstrap_nodes[0].ip().is_loopback());
    assert_eq!(config.bootstrap_nodes[0].port(), 49003);

    let default_options = DhtEngineOptions::from(&DownloadOptions {
        async_dns: false,
        ..Default::default()
    });
    let default_config = build_dht_engine_config_for_family_with_dns_cache(
        &default_options,
        &policy,
        &dns_cache,
        false,
    )
    .await
    .expect("default bootstrap should keep the system resolver path")
    .expect("direct DHT should be available");
    assert!(default_config.bootstrap_nodes.is_empty());
    assert!(default_config.bootstrap_on_start);
}

#[tokio::test]
async fn separate_ipv4_host_and_port_enter_the_bootstrap_list() {
    let options = DownloadOptions {
        dht_entry_point_host: Some("127.0.0.1".to_string()),
        dht_entry_point_port: Some(49004),
        ..Default::default()
    };

    let config = build_dht_engine_config(&options)
        .await
        .expect("IPv4 DHT host and port should build a protocol config");

    assert_eq!(
        config.bootstrap_nodes,
        vec!["127.0.0.1:49004".parse::<SocketAddr>().unwrap()]
    );
}

#[test]
fn ipv6_path_does_not_fall_back_to_ipv4_path() {
    let options = DownloadOptions {
        dht_file_path: Some("v4.dat".to_string()),
        ..Default::default()
    };
    let options = DhtEngineOptions::from(&options);

    assert_eq!(selected_file_path(&options, true), None);
    assert_eq!(selected_file_path(&options, false), Some("v4.dat"));
}

#[test]
fn home_placeholder_expands_in_dht_paths() {
    let expanded = expand_home_path("${HOME}/.aria2/dht.dat", Some(Path::new("/srv/user")));
    assert_eq!(expanded, PathBuf::from("/srv/user/.aria2/dht.dat"));
}
