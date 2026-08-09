mod e2e_helpers;

use aria2_core::engine::command::Command;
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use e2e_helpers::mock_http_server::{MockHttpServer, Response, empty_body};

#[tokio::test]
async fn test_http_max_tries_counts_total_get_attempts() {
    let server = MockHttpServer::start()
        .await
        .expect("mock HTTP server should start");
    server.on_get("/max-tries.bin", |request| {
        if request.method().as_str() == "HEAD" {
            return Response::builder()
                .status(200)
                .header("content-length", "4")
                .body(empty_body())
                .unwrap();
        }

        Response::builder()
            .status(503)
            .header("content-length", "0")
            .body(empty_body())
            .unwrap()
    });

    let dir = tempfile::tempdir().unwrap();
    let options = DownloadOptions {
        max_retries: 2,
        retry_wait: 0,
        ..DownloadOptions::default()
    };
    let url = format!("{}/max-tries.bin", server.base_url());
    let mut command = DownloadCommand::new(
        GroupId::new(0x900),
        &url,
        &options,
        dir.path().to_str(),
        None,
    )
    .expect("HTTP command should construct");

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("retry test should not hang");
    assert!(result.is_err(), "persistent 503 should fail");

    let get_attempts = server
        .take_request_log()
        .into_iter()
        .filter(|request| request.method == "GET" && request.path == "/max-tries.bin")
        .count();
    assert_eq!(get_attempts, 2, "max-tries=2 means two total GET attempts");
    server.shutdown().await;
}
