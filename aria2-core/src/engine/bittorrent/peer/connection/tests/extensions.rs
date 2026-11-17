use super::*;

#[tokio::test]
async fn test_bt_peer_conn_sends_configured_peer_agent_on_wire() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    let (server, endpoint) = listener.accept().await.unwrap();
    let peer = aria2_protocol::bittorrent::peer::connection::PeerConnection::from_stream_with_peer_capabilities(
        server, [0u8; 20], false, false, true,
    );
    let mut connection = BtPeerConn::from_incoming_tcp(peer, endpoint);

    connection
        .send_extension_handshake("contract-agent/1")
        .await
        .unwrap();

    let mut frame_length = [0u8; 4];
    tokio::io::AsyncReadExt::read_exact(&mut client, &mut frame_length)
        .await
        .unwrap();
    let payload_length = u32::from_be_bytes(frame_length) as usize;
    let mut frame = Vec::with_capacity(4 + payload_length);
    frame.extend_from_slice(&frame_length);
    let mut payload = vec![0u8; payload_length];
    tokio::io::AsyncReadExt::read_exact(&mut client, &mut payload)
        .await
        .unwrap();
    frame.extend_from_slice(&payload);

    let message = aria2_protocol::bittorrent::message::factory::parse_message(&frame).unwrap();
    match message {
        Some(aria2_protocol::bittorrent::message::types::BtMessage::Extended {
            ext_id,
            payload,
        }) => {
            assert_eq!(ext_id, 0);
            let handshake =
                aria2_protocol::bittorrent::message::extension::ExtensionHandshake::from_bytes(
                    &payload,
                )
                .unwrap();
            assert_eq!(handshake.v(), Some("contract-agent/1"));
        }
        other => panic!("expected extension handshake, got {other:?}"),
    }
}

#[tokio::test]
async fn test_bt_peer_conn_registers_remote_extension_ids() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    let (server, endpoint) = listener.accept().await.unwrap();
    let peer = aria2_protocol::bittorrent::peer::connection::PeerConnection::from_stream_with_peer_capabilities(
        server, [0u8; 20], false, false, true,
    );
    let mut connection = BtPeerConn::from_incoming_tcp(peer, endpoint);
    connection.allocate_session_resource(16 * 1024, 1, 16 * 1024);

    let mut handshake = aria2_protocol::bittorrent::message::extension::ExtensionHandshake::new();
    handshake
        .with_version("remote-agent/2.3")
        .with_ut_metadata(7)
        .with_ut_pex(9);
    let frame = aria2_protocol::bittorrent::message::serializer::serialize(
        &aria2_protocol::bittorrent::message::types::BtMessage::Extended {
            ext_id: 0,
            payload: handshake.to_bytes(),
        },
    );
    tokio::io::AsyncWriteExt::write_all(&mut client, &frame)
        .await
        .unwrap();

    assert!(connection.read_message().await.unwrap().is_some());
    assert_eq!(connection.peer_extension_id("ut_metadata"), Some(7));
    assert_eq!(connection.peer_extension_id("ut_pex"), Some(9));
    assert_eq!(
        *connection
            .remote_client
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        Some("remote-agent/2.3".to_string())
    );
}

#[tokio::test]
async fn bt_peer_conn_applies_incremental_extension_handshakes_and_disables_zero_ids() {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
    use aria2_protocol::bittorrent::message::serializer::serialize;
    use aria2_protocol::bittorrent::message::types::BtMessage;

    fn extension_handshake(extensions: &[(&[u8], u8)], client: Option<&str>) -> Vec<u8> {
        let mut m = BTreeMap::new();
        for (name, id) in extensions {
            m.insert(name.to_vec(), BencodeValue::Int(i64::from(*id)));
        }
        let mut root = BTreeMap::new();
        root.insert(b"m".to_vec(), BencodeValue::Dict(m));
        if let Some(client) = client {
            root.insert(
                b"v".to_vec(),
                BencodeValue::Bytes(client.as_bytes().to_vec()),
            );
        }
        BencodeValue::Dict(root).encode()
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    let (server, endpoint) = listener.accept().await.unwrap();
    let peer = aria2_protocol::bittorrent::peer::connection::PeerConnection::from_stream_with_peer_capabilities(
        server, [0u8; 20], false, false, true,
    );
    let mut connection = BtPeerConn::from_incoming_tcp(peer, endpoint);
    connection.allocate_session_resource(16 * 1024, 1, 16 * 1024);

    for (extensions, client_version) in [
        (
            vec![(b"ut_metadata".as_slice(), 7), (b"ut_pex".as_slice(), 19)],
            Some("remote-agent/2.3"),
        ),
        (vec![(b"ut_metadata".as_slice(), 8)], None),
        (vec![(b"ut_pex".as_slice(), 0)], None),
    ] {
        let frame = serialize(&BtMessage::Extended {
            ext_id: 0,
            payload: extension_handshake(&extensions, client_version),
        });
        tokio::io::AsyncWriteExt::write_all(&mut client, &frame)
            .await
            .unwrap();
        assert!(connection.read_message().await.unwrap().is_some());
    }

    assert_eq!(connection.peer_extension_id("ut_metadata"), Some(8));
    assert_eq!(
        *connection
            .remote_client
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        Some("remote-agent/2.3".to_string()),
        "partial BEP 10 updates without v preserve the learned client name"
    );
    assert_eq!(
        connection.peer_extension_id("ut_pex"),
        None,
        "BEP 10 extension ID 0 disables that extension"
    );
}

#[tokio::test]
async fn bt_peer_conn_accepts_extension_handshake_without_m_dictionary() {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
    use aria2_protocol::bittorrent::message::serializer::serialize;
    use aria2_protocol::bittorrent::message::types::BtMessage;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    let (server, endpoint) = listener.accept().await.unwrap();
    let peer = aria2_protocol::bittorrent::peer::connection::PeerConnection::from_stream_with_peer_capabilities(
        server, [0u8; 20], false, false, true,
    );
    let mut connection = BtPeerConn::from_incoming_tcp(peer, endpoint);
    connection.allocate_session_resource(16 * 1024, 1, 16 * 1024);

    let payload = BencodeValue::Dict(BTreeMap::from([(
        b"v".to_vec(),
        BencodeValue::Bytes(b"remote-agent/2.3".to_vec()),
    )]))
    .encode();
    let frame = serialize(&BtMessage::Extended { ext_id: 0, payload });
    tokio::io::AsyncWriteExt::write_all(&mut client, &frame)
        .await
        .unwrap();

    assert!(matches!(
        connection.read_message().await.unwrap(),
        Some(BtMessage::Extended { ext_id: 0, .. })
    ));
    assert_eq!(
        *connection
            .remote_client
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        Some("remote-agent/2.3".to_string()),
        "the optional m dictionary must not suppress the other handshake fields"
    );
    assert_eq!(connection.peer_extension_id("ut_metadata"), None);
    assert_eq!(connection.peer_extension_id("ut_pex"), None);
}

#[tokio::test]
async fn bt_peer_conn_keeps_other_fields_when_extension_map_has_wrong_type() {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
    use aria2_protocol::bittorrent::message::serializer::serialize;
    use aria2_protocol::bittorrent::message::types::BtMessage;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    let (server, endpoint) = listener.accept().await.unwrap();
    let peer = aria2_protocol::bittorrent::peer::connection::PeerConnection::from_stream_with_peer_capabilities(
        server, [0u8; 20], false, false, true,
    );
    let mut connection = BtPeerConn::from_incoming_tcp(peer, endpoint);
    connection.allocate_session_resource(16 * 1024, 1, 16 * 1024);

    let payload = BencodeValue::Dict(BTreeMap::from([
        (b"m".to_vec(), BencodeValue::Int(42)),
        (b"p".to_vec(), BencodeValue::Int(6881)),
        (
            b"v".to_vec(),
            BencodeValue::Bytes(b"remote-agent/2.3".to_vec()),
        ),
    ]))
    .encode();
    let frame = serialize(&BtMessage::Extended { ext_id: 0, payload });
    tokio::io::AsyncWriteExt::write_all(&mut client, &frame)
        .await
        .unwrap();

    assert!(matches!(
        connection.read_message().await.unwrap(),
        Some(BtMessage::Extended { ext_id: 0, .. })
    ));
    assert_eq!(connection.remote_listen_port, Some(6881));
    assert_eq!(
        *connection
            .remote_client
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        Some("remote-agent/2.3".to_string()),
        "an invalid optional m field must not suppress valid BEP 10 fields"
    );
    assert_eq!(connection.peer_extension_id("ut_metadata"), None);
    assert_eq!(connection.peer_extension_id("ut_pex"), None);
}

#[tokio::test]
async fn bt_peer_conn_enforces_extended_messaging_handshake_capability() {
    use aria2_protocol::bittorrent::message::extension::ExtensionHandshake;
    use aria2_protocol::bittorrent::message::handshake::Handshake;
    use aria2_protocol::bittorrent::message::serializer::serialize;
    use aria2_protocol::bittorrent::message::types::BtMessage;

    let info_hash = [0x42u8; 20];
    for extended_messaging in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let incoming = tokio::spawn(async move {
            let (stream, endpoint) = listener.accept().await.unwrap();
            let incoming =
                aria2_protocol::bittorrent::peer::incoming::receive(stream, &[info_hash])
                    .await
                    .unwrap();
            let peer = incoming.complete([0x24u8; 20], None, false).await.unwrap();
            let mut connection = BtPeerConn::from_incoming_tcp(peer, endpoint);
            connection.allocate_session_resource(16 * 1024, 1, 16 * 1024);
            connection
        });

        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut handshake = Handshake::new(&info_hash, &[0x23u8; 20]);
        handshake.reserved[5] &= !0x10;
        if extended_messaging {
            handshake.reserved[5] |= 0x10;
        }
        tokio::io::AsyncWriteExt::write_all(&mut client, &handshake.to_bytes())
            .await
            .unwrap();
        let mut response = [0u8; 68];
        tokio::io::AsyncReadExt::read_exact(&mut client, &mut response)
            .await
            .unwrap();
        assert!(
            Handshake::parse(&response)
                .unwrap()
                .supports_extended_messaging()
        );

        let mut connection = incoming.await.unwrap();
        let mut extension_handshake = ExtensionHandshake::new();
        extension_handshake.with_ut_pex(19);
        let frame = serialize(&BtMessage::Extended {
            ext_id: 0,
            payload: extension_handshake.to_bytes(),
        });
        tokio::io::AsyncWriteExt::write_all(&mut client, &frame)
            .await
            .unwrap();

        if extended_messaging {
            assert!(connection.read_message().await.unwrap().is_some());
            assert_eq!(connection.peer_extension_id("ut_pex"), Some(19));
        } else {
            let error = connection.read_message().await.unwrap_err().to_string();
            assert!(error.contains("extended message without negotiating extended messaging"));
            assert_eq!(connection.peer_extension_id("ut_pex"), None);
        }
    }
}

#[tokio::test]
async fn test_bt_peer_conn_initializes_fast_extension_from_handshake() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let _client = tokio::net::TcpStream::connect(address).await.unwrap();
    let (server, endpoint) = listener.accept().await.unwrap();
    let peer = aria2_protocol::bittorrent::peer::connection::PeerConnection::from_stream_with_peer(
        server, [0u8; 20], false, true,
    );
    let mut connection = BtPeerConn::from_incoming_tcp(peer, endpoint);

    connection.allocate_session_resource(16 * 1024, 1, 16 * 1024);

    assert!(connection.is_fast_extension_enabled());
    connection.add_peer_allowed_fast(0);
    connection.add_am_allowed_fast(1);
    assert!(connection.peer_allowed_fast_set().contains(&0));
    assert!(!connection.peer_allowed_fast_set().contains(&1));
    assert!(connection.allowed_fast_set().contains(&0));
    assert!(connection.am_allowed_fast(1));
    assert!(!connection.am_allowed_fast(0));
}

// -----------------------------------------------------------------------
// BtPeerConn — queue_message and flush (unit test of buffer logic)
// -----------------------------------------------------------------------
