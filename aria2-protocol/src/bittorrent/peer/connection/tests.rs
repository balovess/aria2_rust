use super::*;

async fn connect_for_test(
    addr: &PeerAddr,
    info_hash_v1: &[u8; 20],
    info_hash_v2: Option<&[u8; 32]>,
    local_peer_id: &[u8; 20],
    timeout: std::time::Duration,
    dht_enabled: bool,
) -> Result<PeerConnection, String> {
    let socket_addr = addr
        .to_socket_addr()
        .map_err(|error| format!("invalid peer address: {error}"))?;
    let stream = TcpStream::connect(socket_addr)
        .await
        .map_err(|error| format!("peer connection failed: {error}"))?;
    PeerConnection::connect_with_stream(
        stream,
        socket_addr,
        info_hash_v1,
        info_hash_v2,
        local_peer_id,
        timeout,
        dht_enabled,
    )
    .await
}

#[test]
fn test_peer_addr_compact_roundtrip() {
    let addr = PeerAddr::new("192.168.1.100", 6881);
    let compact = addr.to_compact();
    let parsed = PeerAddr::from_compact(&compact).unwrap();
    assert_eq!(parsed.ip, addr.ip);
    assert_eq!(parsed.port, addr.port);
}

#[test]
fn test_peer_addr_socket_conversion_supports_ipv6() {
    let addr = PeerAddr::new("2001:db8::1", 6881);
    assert_eq!(
        addr.to_socket_addr().unwrap().to_string(),
        "[2001:db8::1]:6881"
    );
}

#[test]
fn test_peer_addr_socket_conversion_rejects_invalid_ip() {
    let addr = PeerAddr::new("not-an-ip", 6881);
    assert!(addr.to_socket_addr().is_err());
}

#[test]
fn test_peer_addr_from_compact() {
    let data: [u8; 6] = [127, 0, 0, 1, 0x1A, 0x0B];
    let addr = PeerAddr::from_compact(&data).unwrap();
    assert_eq!(addr.ip, "127.0.0.1");
    assert_eq!(addr.port, 6667);
}

#[test]
fn test_peer_addr_too_short() {
    assert!(PeerAddr::from_compact(&[1, 2, 3]).is_none());
}

#[test]
fn test_peer_addr_from_compact_v6() {
    // ::1 (loopback) + port 6881
    let mut data = [0u8; 18];
    data[15] = 1; // ::1 in 16 bytes
    data[16..18].copy_from_slice(&6881u16.to_be_bytes());
    let addr = PeerAddr::from_compact_v6(&data).unwrap();
    assert_eq!(addr.ip, "::1");
    assert_eq!(addr.port, 6881);
}

#[test]
fn test_peer_addr_compact_v6_roundtrip() {
    let addr = PeerAddr::new("2001:db8::1", 6881);
    let compact = addr.to_compact_v6().unwrap();
    let parsed = PeerAddr::from_compact_v6(&compact).unwrap();
    assert_eq!(parsed.ip, addr.ip);
    assert_eq!(parsed.port, addr.port);
}

#[test]
fn test_peer_addr_compact_v6_too_short() {
    assert!(PeerAddr::from_compact_v6(&[0u8; 17]).is_none());
}

#[test]
fn test_peer_addr_to_compact_v6_non_ipv6() {
    let addr = PeerAddr::new("192.168.1.1", 6881);
    assert!(addr.to_compact_v6().is_none());
}

#[test]
fn test_peer_addr_from_compact_v6_full_addr() {
    // 2001:0db8:85a3:0000:0000:8a2e:0370:7334 + port 1234
    let mut data = [0u8; 18];
    data[0..2].copy_from_slice(&[0x20, 0x01]);
    data[2..4].copy_from_slice(&[0x0d, 0xb8]);
    data[4..6].copy_from_slice(&[0x85, 0xa3]);
    data[6..8].copy_from_slice(&[0x00, 0x00]);
    data[8..10].copy_from_slice(&[0x00, 0x00]);
    data[10..12].copy_from_slice(&[0x8a, 0x2e]);
    data[12..14].copy_from_slice(&[0x03, 0x70]);
    data[14..16].copy_from_slice(&[0x73, 0x34]);
    data[16..18].copy_from_slice(&1234u16.to_be_bytes());

    let addr = PeerAddr::from_compact_v6(&data).unwrap();
    assert_eq!(addr.ip, "2001:db8:85a3::8a2e:370:7334");
    assert_eq!(addr.port, 1234);
}

#[tokio::test]
async fn test_read_message_preserves_partial_frame_after_cancellation() {
    use tokio::io::AsyncWriteExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    let (server, _) = listener.accept().await.unwrap();
    let mut connection = PeerConnection::from_stream_with_peer(server, [0u8; 20], false, false);
    let frame = crate::bittorrent::message::serializer::serialize(&BtMessage::Choke);

    client.write_all(&frame[..2]).await.unwrap();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(25),
            connection.read_message()
        )
        .await
        .is_err()
    );

    client.write_all(&frame[2..]).await.unwrap();
    assert_eq!(
        connection.read_message().await.unwrap(),
        Some(BtMessage::Choke)
    );
}

#[tokio::test]
async fn selected_stream_handshake_uses_configured_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    });

    let addr = PeerAddr::new("127.0.0.1", address.port());
    let result = connect_for_test(
        &addr,
        &[1u8; 20],
        None,
        &[b'X'; 20],
        std::time::Duration::from_millis(20),
        false,
    )
    .await;

    match result {
        Err(error) => assert!(error.contains("Handshake response timeout")),
        Ok(_) => panic!("the peer does not answer the handshake"),
    }
    server.abort();
}

#[tokio::test]
async fn selected_stream_handshake_sends_the_configured_peer_id() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 68];
        tokio::io::AsyncReadExt::read_exact(&mut stream, &mut request)
            .await
            .unwrap();
        let received = Handshake::parse(&request).unwrap();
        assert_eq!(received.peer_id, [b'X'; 20]);
        let response = Handshake::new(&[1u8; 20], &[b'Y'; 20]).to_bytes();
        tokio::io::AsyncWriteExt::write_all(&mut stream, &response)
            .await
            .unwrap();
    });

    let addr = PeerAddr::new("127.0.0.1", address.port());
    let connection = connect_for_test(
        &addr,
        &[1u8; 20],
        None,
        &[b'X'; 20],
        std::time::Duration::from_secs(1),
        false,
    )
    .await
    .unwrap();

    assert_eq!(connection.remote_peer_id(), Some(&[b'Y'; 20]));
    server.await.unwrap();
}

#[tokio::test]
async fn selected_stream_handshake_preserves_dht_capabilities() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 68];
        stream.read_exact(&mut request).await.unwrap();
        let request = Handshake::parse(&request).unwrap();
        assert!(request.supports_dht());
        let response = Handshake::new(&[1u8; 20], &[b'Y'; 20])
            .with_dht(true)
            .to_bytes();
        stream.write_all(&response).await.unwrap();
    });

    let connection = connect_for_test(
        &PeerAddr::new("127.0.0.1", address.port()),
        &[1u8; 20],
        None,
        &[b'X'; 20],
        std::time::Duration::from_secs(1),
        true,
    )
    .await
    .unwrap();

    assert!(connection.remote_supports_dht());
    assert!(connection.remote_supports_fast_extension());
    server.await.unwrap();
}

#[tokio::test]
async fn hybrid_connect_accepts_v2_hash_in_response() {
    let v1 = [1u8; 20];
    let v2 = [2u8; 32];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 68];
        stream.read_exact(&mut request).await.unwrap();
        let request = Handshake::parse(&request).unwrap();
        assert_eq!(request.info_hash, v1);
        assert!(request.supports_bep52());
        let response = Handshake::new(&v2[..20].try_into().unwrap(), &[b'Y'; 20])
            .with_bep52(true)
            .to_bytes();
        stream.write_all(&response).await.unwrap();
    });

    let connection = connect_for_test(
        &PeerAddr::new("127.0.0.1", address.port()),
        &v1,
        Some(&v2),
        &[b'X'; 20],
        std::time::Duration::from_secs(1),
        false,
    )
    .await
    .unwrap();
    assert_eq!(connection.remote_peer_id(), Some(&[b'Y'; 20]));
    server.await.unwrap();
}

#[tokio::test]
async fn hybrid_connect_rejects_v2_response_without_capability_bit() {
    let v1 = [1u8; 20];
    let v2 = [2u8; 32];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 68];
        stream.read_exact(&mut request).await.unwrap();
        let response = Handshake::new(&v2[..20].try_into().unwrap(), &[b'Y'; 20]);
        stream.write_all(&response.to_bytes()).await.unwrap();
    });

    let result = connect_for_test(
        &PeerAddr::new("127.0.0.1", address.port()),
        &v1,
        Some(&v2),
        &[b'X'; 20],
        std::time::Duration::from_secs(1),
        false,
    )
    .await;
    match result {
        Err(error) => assert!(error.contains("missing BEP 52 capability")),
        Ok(_) => panic!("v2 response without the capability bit must be rejected"),
    }
    server.await.unwrap();
}
