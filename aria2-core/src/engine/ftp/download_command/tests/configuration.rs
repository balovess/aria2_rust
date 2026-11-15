use super::*;
use tracing_subscriber::layer::{Layer, SubscriberExt};

#[test]
fn command_creation_log_does_not_expose_uri_credentials() {
    let output = Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer_output = Arc::clone(&output);
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .without_time()
            .with_ansi(false)
            .with_writer(move || SharedBufferWriter(Arc::clone(&writer_output)))
            .with_filter(tracing_subscriber::filter::dynamic_filter_fn(
                |metadata, _context| *metadata.level() <= tracing::Level::INFO,
            )),
    );

    let command_result = tracing::subscriber::with_default(subscriber, || {
        FtpDownloadCommand::new(
            GroupId::new(100),
            "ftp://private-user:private-password@example.test/private-file",
            &DownloadOptions::default(),
            None,
            None,
        )
    });
    assert!(command_result.is_ok());

    let output = String::from_utf8(output.lock().unwrap().clone()).unwrap();
    assert!(
        output.contains("example.test:21"),
        "captured FTP command log did not include the endpoint: {output:?}"
    );
    assert!(!output.contains("private-user"));
    assert!(!output.contains("private-password"));
}

#[test]
fn test_parse_uri_simple() {
    let uri = "ftp://example.com/file.txt";
    let result = FtpDownloadCommand::parse_uri(uri).unwrap();
    assert_eq!(result.host, "example.com");
    assert_eq!(result.port, 21);
    assert_eq!(result.username, "anonymous");
    assert_eq!(result.password, "aria2@");
    assert_eq!(result.remote_path, "/file.txt");
    assert!(!result.secure);
}

#[test]
fn test_retry_policy_comes_from_download_options() {
    let options = DownloadOptions {
        max_retries: 7,
        retry_wait: 3,
        ..DownloadOptions::default()
    };
    let command = FtpDownloadCommand::new(
        GroupId::new(101),
        "ftp://example.com/file.txt",
        &options,
        None,
        None,
    )
    .unwrap();

    assert_eq!(command.retry_policy.max_tries(), 7);
    assert_eq!(command.retry_policy.base_wait_ms, 3000);
}

#[test]
fn ftp_proxy_rpc_options_reach_the_core_proxy_configuration() {
    let rpc_options = HashMap::from([
        (
            "ftp-proxy".to_string(),
            serde_json::json!("http://proxy.example:3128"),
        ),
        (
            "ftp-proxy-user".to_string(),
            serde_json::json!("proxy-user"),
        ),
        (
            "ftp-proxy-passwd".to_string(),
            serde_json::json!("proxy-password"),
        ),
        ("proxy-method".to_string(), serde_json::json!("get")),
        (
            "user-agent".to_string(),
            serde_json::json!("task-agent/1.0"),
        ),
    ]);
    let options = DownloadOptions::try_from_rpc_options(&rpc_options)
        .expect("RPC options should use the registered option parser");
    let command = FtpDownloadCommand::new(
        GroupId::new(102),
        "ftp://download-user@example.com/file.bin",
        &options,
        None,
        None,
    )
    .expect("FTP command should accept valid proxy options");

    let (proxy, method) = command
        .ftp_proxy_config()
        .expect("FTP proxy configuration should be valid")
        .expect("FTP proxy should be configured");
    assert_eq!(proxy.proxy_host, "proxy.example");
    assert_eq!(proxy.proxy_port, 3128);
    assert_eq!(proxy.proxy_username, "proxy-user");
    assert_eq!(proxy.proxy_password, "proxy-password");
    assert_eq!(proxy.ftp_username, "download-user");
    assert_eq!(proxy.user_agent, "task-agent/1.0");
    assert_eq!(method, crate::ftp::connection::ProxyMethod::Get);
}

#[test]
fn ftp_no_proxy_uses_exact_host_matching_for_proxy_selection() {
    let options = DownloadOptions {
        ftp_proxy: Some("http://proxy.example:3128".to_string()),
        no_proxy: Some("example.test".to_string()),
        ..DownloadOptions::default()
    };
    let exact_host = FtpDownloadCommand::new(
        GroupId::new(103),
        "ftp://example.test/file.bin",
        &options,
        None,
        None,
    )
    .unwrap();
    assert!(exact_host.ftp_proxy_config().unwrap().is_none());

    let subdomain = FtpDownloadCommand::new(
        GroupId::new(104),
        "ftp://download.example.test/file.bin",
        &options,
        None,
        None,
    )
    .unwrap();
    assert!(subdomain.ftp_proxy_config().unwrap().is_some());
}
