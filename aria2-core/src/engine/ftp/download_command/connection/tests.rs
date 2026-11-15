use super::*;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

#[tokio::test]
async fn request_timeout_overrides_ftp_command_data_and_completion_fallbacks() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind FTP timeout test listener");
    let address = listener.local_addr().expect("read listener address");
    let client = TcpStream::connect(address)
        .await
        .expect("connect FTP timeout test stream");
    let (server, _) = listener
        .accept()
        .await
        .expect("accept FTP timeout test stream");
    let peer = client.peer_addr().expect("read FTP peer address");
    let connection = ConnectionContext::new("ftp.test", 21, peer);
    let mut control = RawFtpControl::from_stream(
        FtpControlStream::Plain(client),
        "ftp.test",
        connection,
        None,
        Arc::new(OutboundNetworkPolicy::direct()),
    );

    assert_eq!(
        control.command_timeout(),
        Duration::from_secs(constants::FTP_COMMAND_TIMEOUT_SECS)
    );
    assert_eq!(
        control.data_connection_timeout(),
        Duration::from_secs(constants::FTP_DATA_CONNECTION_TIMEOUT_SECS)
    );
    assert_eq!(
        control.transfer_completion_timeout(),
        Duration::from_secs(constants::FTP_TRANSFER_COMPLETE_TIMEOUT_SECS)
    );

    let configured_timeout = Duration::from_secs(7);
    control.set_request_timeout(Some(configured_timeout));
    assert_eq!(control.command_timeout(), configured_timeout);
    assert_eq!(control.data_connection_timeout(), configured_timeout);
    assert_eq!(control.transfer_completion_timeout(), configured_timeout);

    drop(control);
    drop(server);
}

#[tokio::test]
async fn configured_request_timeout_bounds_ftp_control_response() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind FTP timeout test listener");
    let address = listener.local_addr().expect("read listener address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener
            .accept()
            .await
            .expect("accept FTP timeout test stream");
        let mut reader = BufReader::new(stream);
        let mut command = String::new();
        reader
            .read_line(&mut command)
            .await
            .expect("read TYPE command");
        assert_eq!(command, "TYPE I\r\n");
        tokio::time::sleep(Duration::from_millis(120)).await;
    });

    let client = TcpStream::connect(address)
        .await
        .expect("connect FTP timeout test stream");
    let peer = client.peer_addr().expect("read FTP peer address");
    let mut control = RawFtpControl::from_stream(
        FtpControlStream::Plain(client),
        "ftp.test",
        ConnectionContext::new("ftp.test", 21, peer),
        None,
        Arc::new(OutboundNetworkPolicy::direct()),
    );
    control.set_request_timeout(Some(Duration::from_millis(30)));

    let started_at = Instant::now();
    let result = timeout(Duration::from_millis(100), control.set_transfer_type("I"))
        .await
        .expect("configured FTP timeout should fire before the outer guard");
    assert!(result.is_err(), "FTP command unexpectedly succeeded");
    assert!(started_at.elapsed() < Duration::from_millis(100));
    server.await.expect("FTP timeout test server should finish");
}

#[tokio::test]
async fn configured_request_timeout_bounds_ftp_greeting() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind FTP greeting timeout test listener");
    let address = listener
        .local_addr()
        .expect("read FTP greeting timeout listener address");
    let server = tokio::spawn(async move {
        let (_stream, _) = listener
            .accept()
            .await
            .expect("accept FTP greeting timeout test stream");
        tokio::time::sleep(Duration::from_millis(120)).await;
    });

    let started_at = Instant::now();
    let result = timeout(
        Duration::from_millis(100),
        RawFtpControl::connect_at(
            "127.0.0.1",
            address.port(),
            address,
            Arc::new(OutboundNetworkPolicy::direct()),
            Some(Duration::from_millis(30)),
        ),
    )
    .await
    .expect("configured FTP timeout should fire before the outer guard");
    assert!(result.is_err(), "FTP greeting unexpectedly succeeded");
    assert!(started_at.elapsed() < Duration::from_millis(100));
    server
        .await
        .expect("FTP greeting timeout server should finish");
}
