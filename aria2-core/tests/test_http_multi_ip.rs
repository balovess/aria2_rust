mod e2e_helpers;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use aria2_core::engine::command::Command;
use aria2_core::engine::http::download_command::DownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use aria2_core::util::rwlock_ext::RwLockRecover;
use e2e_helpers::mock_http_server::MockHttpServer;

fn range_requests(server: &MockHttpServer) -> usize {
    server
        .take_request_log()
        .iter()
        .filter(|request| {
            request
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("range"))
        })
        .count()
}

#[tokio::test]
async fn concurrent_ranges_use_multiple_resolved_ips() {
    let body = Arc::new(
        (0..(8 * 1024 * 1024))
            .map(|index| index as u8)
            .collect::<Vec<_>>(),
    );
    let first_server = MockHttpServer::start().await.unwrap();
    let port = first_server.addr().port();
    let second_server = MockHttpServer::start_bound(format!("127.0.0.2:{port}").parse().unwrap())
        .await
        .unwrap();
    first_server.register_range_response("/file", &body);
    second_server.register_range_response("/file", &body);

    let uri = format!("http://multi-ip.test:{port}/file");
    let resolved = vec![
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)), port),
    ];
    let options = DownloadOptions {
        split: Some(4),
        max_connection_per_server: Some(4),
        use_multiple_ips: true,
        file_allocation: Some("none".into()),
        ..DownloadOptions::default()
    };
    let output_dir = tempfile::tempdir().unwrap();
    let output_name = "multi-ip.bin";
    let output_dir_arg = output_dir.path().to_string_lossy().into_owned();
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(rand::random::<u64>()),
        vec![uri.clone()],
        options.clone(),
    )));
    group.recover().set_total_length(body.len() as u64);
    let mut command = DownloadCommand::new_with_group_and_resolved_addresses(
        group,
        &uri,
        &options,
        Some(&output_dir_arg),
        Some(output_name),
        Some(resolved),
    )
    .unwrap();
    command.group_mut().set_option_snapshot(HashMap::from([(
        "min-split-size".to_string(),
        serde_json::json!("1M"),
    )]));
    command.execute().await.unwrap();

    let first_ranges = range_requests(&first_server);
    let second_ranges = range_requests(&second_server);
    assert!(
        first_ranges > 0,
        "first DNS address received no Range request"
    );
    assert!(
        second_ranges > 0,
        "second DNS address received no Range request"
    );
    assert_eq!(
        tokio::fs::read(output_dir.path().join(output_name))
            .await
            .unwrap(),
        body.as_slice()
    );
}
