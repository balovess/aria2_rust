use super::*;

#[tokio::test]
async fn test_raw_ftp_control_connect_invalid_address() {
    let result = RawFtpControl::connect_at(
        "invalid.host.name.invalid",
        21,
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(crate::network::OutboundNetworkPolicy::direct()),
        None,
    )
    .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn ftp_control_binding_reaches_the_server_socket() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("FTP fixture should bind");
    let address = listener.local_addr().expect("FTP fixture address");
    let server = tokio::spawn(async move {
        let (mut stream, peer) = listener.accept().await.expect("FTP client should connect");
        stream
            .write_all(b"220 test FTP server\r\n")
            .await
            .expect("write FTP greeting");
        peer.ip()
    });

    let control = RawFtpControl::connect_at(
        "127.0.0.1",
        address.port(),
        address,
        Arc::new(crate::network::OutboundNetworkPolicy::single(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        )),
        None,
    )
    .await
    .expect("FTP control connection should use the configured source address");

    assert_eq!(
        control.connection_context().peer_addr.ip(),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
    assert_eq!(
        server.await.expect("FTP fixture should finish"),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
}

#[tokio::test]
async fn ftp_control_reuses_one_policy_bound_connection_for_multiple_commands() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("FTP reuse fixture should bind");
    let address = listener.local_addr().expect("FTP reuse fixture address");
    let server = tokio::spawn(async move {
        let (stream, peer) = listener.accept().await.expect("FTP client should connect");
        let mut reader = BufReader::new(stream);
        reader
            .get_mut()
            .write_all(b"220 reuse fixture\r\n")
            .await
            .expect("write FTP greeting");
        for _ in 0..2 {
            let mut line = Vec::new();
            reader
                .read_until(b'\n', &mut line)
                .await
                .expect("read FTP command");
            assert!(line.starts_with(b"NOOP ") || line.starts_with(b"NOOP\r"));
            reader
                .get_mut()
                .write_all(b"200 NOOP ok\r\n")
                .await
                .expect("write FTP command response");
        }
        peer
    });

    let mut control = RawFtpControl::connect_at(
        "127.0.0.1",
        address.port(),
        address,
        Arc::new(crate::network::OutboundNetworkPolicy::single(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        )),
        None,
    )
    .await
    .expect("FTP control connection should be established");
    assert_eq!(control.command("NOOP 1").await.unwrap().0, 200);
    assert_eq!(control.command("NOOP 2").await.unwrap().0, 200);
    assert_eq!(
        server.await.unwrap().ip(),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
}

#[tokio::test]
async fn ftp_ipv4_passive_mode_uses_pasv_without_epsv_probe() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    let data_listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("FTP data fixture should bind");
    let data_port = data_listener
        .local_addr()
        .expect("FTP data fixture address")
        .port();
    let control_listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("FTP control fixture should bind");
    let control_address = control_listener
        .local_addr()
        .expect("FTP control fixture address");

    let server = tokio::spawn(async move {
        let (stream, _) = control_listener
            .accept()
            .await
            .expect("FTP control connection should arrive");
        let mut reader = BufReader::new(stream);
        reader
            .get_mut()
            .write_all(b"220 FTP fixture ready\r\n")
            .await
            .expect("write greeting");

        let mut command = String::new();
        reader
            .read_line(&mut command)
            .await
            .expect("read passive command");
        assert_eq!(command.trim_end(), "PASV");
        reader
            .get_mut()
            .write_all(
                format!(
                    "227 Entering Passive Mode (127,0,0,1,{},{})\r\n",
                    data_port / 256,
                    data_port % 256
                )
                .as_bytes(),
            )
            .await
            .expect("write PASV response");

        data_listener
            .accept()
            .await
            .expect("FTP data connection should use the advertised port");
    });

    let mut control = RawFtpControl::connect_at(
        "127.0.0.1",
        control_address.port(),
        control_address,
        Arc::new(crate::network::OutboundNetworkPolicy::direct()),
        None,
    )
    .await
    .expect("FTP control connection should be established");
    let data_stream = control
        .enter_passive_mode()
        .await
        .expect("IPv4 passive mode should use PASV");
    drop(data_stream);
    server.await.expect("FTP fixture should finish");
}
