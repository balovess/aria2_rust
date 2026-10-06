use super::utp_connection::UtpPeerConnection;
use aria2_protocol::bittorrent::utp::{ConnectionState, UtpSocket};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

#[tokio::test]
async fn recv_message_waits_for_fragmented_frame() {
    let info_hash = [7u8; 20];
    let mut client = UtpSocket::bind("127.0.0.1:0").unwrap();
    let mut server = UtpSocket::bind("127.0.0.1:0").unwrap();
    let client_conn_id = client.connect(server.local_addr()).unwrap();

    for _ in 0..100 {
        server.poll_recv().unwrap();
        client.poll_recv().unwrap();
        if client.connection_state(client_conn_id).unwrap() == ConnectionState::Established {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    assert_eq!(
        client.connection_state(client_conn_id).unwrap(),
        ConnectionState::Established
    );
    let server_conn_id = server.connection_ids()[0];
    let client = Arc::new(Mutex::new(client));
    let server = Arc::new(Mutex::new(server));

    let mut peer = UtpPeerConnection::new(client, client_conn_id, info_hash);
    server
        .lock()
        .await
        .send(server_conn_id, &[0, 0, 0, 1])
        .unwrap();

    let receive = tokio::spawn(async move { peer.recv_message().await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    server.lock().await.send(server_conn_id, &[0]).unwrap();

    let result = tokio::time::timeout(Duration::from_secs(1), receive)
        .await
        .expect("fragmented uTP message should complete")
        .expect("receiver task should not panic")
        .expect("receiver should succeed");
    assert_eq!(result, Some(vec![0, 0, 0, 1, 0]));
}

#[tokio::test]
async fn connect_completes_real_bittorrent_handshake_over_utp() {
    use aria2_protocol::bittorrent::message::handshake::Handshake;

    let info_hash = [7u8; 20];
    let local_peer_id = [8u8; 20];
    let remote_peer_id = [9u8; 20];
    let client_socket = Arc::new(Mutex::new(
        UtpSocket::bind("127.0.0.1:0").expect("client uTP socket should bind"),
    ));
    let mut server = UtpSocket::bind("127.0.0.1:0").expect("server uTP socket should bind");
    let server_addr = server.local_addr();

    let server_task = tokio::spawn(async move {
        let mut request_buffer = Vec::new();
        loop {
            let packets = server.poll_recv().expect("server poll should succeed");
            for (conn_id, data) in packets {
                request_buffer.extend_from_slice(&data);
                if request_buffer.len() < 68 {
                    continue;
                }
                let request =
                    Handshake::parse(&request_buffer).expect("client handshake should parse");
                assert_eq!(request.info_hash, info_hash);
                assert_eq!(request.peer_id, local_peer_id);
                assert_eq!(
                    server.connection_state(conn_id).unwrap(),
                    ConnectionState::Established
                );
                let response = Handshake::new(&info_hash, &remote_peer_id).to_bytes();
                server
                    .send(conn_id, &response)
                    .expect("server should send BitTorrent handshake");
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });

    let connection = UtpPeerConnection::connect_with_shared_socket_hybrid(
        client_socket,
        server_addr,
        &info_hash,
        None,
        &local_peer_id,
        Duration::from_secs(1),
        false,
    )
    .await
    .expect("uTP connection should complete the BitTorrent handshake");

    tokio::time::timeout(Duration::from_secs(1), server_task)
        .await
        .expect("server should receive the BitTorrent handshake")
        .expect("server task should not panic");
    assert!(connection.is_connected());
    assert_eq!(connection.remote_peer_id(), Some(remote_peer_id));
    assert!(connection.remote_supports_fast_extension());
    assert!(connection.remote_supports_extended_messaging());
}

#[tokio::test]
async fn connect_accepts_bep52_v2_response_over_utp() {
    use aria2_protocol::bittorrent::message::handshake::Handshake;

    let info_hash_v1 = [17u8; 20];
    let info_hash_v2 = [18u8; 32];
    let local_peer_id = [19u8; 20];
    let remote_peer_id = [20u8; 20];
    let client_socket = Arc::new(Mutex::new(UtpSocket::bind("127.0.0.1:0").unwrap()));
    let mut server = UtpSocket::bind("127.0.0.1:0").unwrap();
    let server_addr = server.local_addr();

    let server_task = tokio::spawn(async move {
        let mut request_buffer = Vec::new();
        loop {
            for (conn_id, data) in server.poll_recv().unwrap() {
                request_buffer.extend_from_slice(&data);
                if request_buffer.len() < 68 {
                    continue;
                }
                let request = Handshake::parse(&request_buffer).unwrap();
                assert_eq!(request.info_hash, info_hash_v1);
                assert!(request.supports_bep52());
                let v2_wire: [u8; 20] = info_hash_v2[..20].try_into().unwrap();
                server
                    .send(
                        conn_id,
                        &Handshake::new(&v2_wire, &remote_peer_id)
                            .with_bep52(true)
                            .to_bytes(),
                    )
                    .unwrap();
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });

    let connection = UtpPeerConnection::connect_with_shared_socket_hybrid(
        client_socket,
        server_addr,
        &info_hash_v1,
        Some(&info_hash_v2),
        &local_peer_id,
        Duration::from_secs(1),
        false,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(1), server_task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(connection.remote_peer_id(), Some(remote_peer_id));
}

#[tokio::test]
async fn connect_retransmits_syn_after_deadline_without_external_polling() {
    use aria2_protocol::bittorrent::message::handshake::Handshake;
    use aria2_protocol::bittorrent::utp::{PacketType, UtpPacket, UtpSocket};

    let info_hash = [27u8; 20];
    let local_peer_id = [28u8; 20];
    let remote_peer_id = [29u8; 20];
    let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer_address = peer.local_addr().unwrap();
    let peer_task = tokio::spawn(async move {
        let mut buffer = [0; 2048];
        let (length, client_address) = peer.recv_from(&mut buffer).await.unwrap();
        let initial_syn = UtpPacket::from_bytes(&buffer[..length]).unwrap();
        assert_eq!(initial_syn.packet_type().unwrap(), PacketType::StSyn);

        let (length, retry_address) =
            tokio::time::timeout(Duration::from_secs(3), peer.recv_from(&mut buffer))
                .await
                .expect("uTP connect loop should retransmit SYN on its timer")
                .unwrap();
        assert_eq!(retry_address, client_address);
        let retried_syn = UtpPacket::from_bytes(&buffer[..length]).unwrap();
        assert_eq!(retried_syn.packet_type().unwrap(), PacketType::StSyn);
        assert_eq!(retried_syn.connection_id, initial_syn.connection_id);
        assert_eq!(retried_syn.seq_nr, initial_syn.seq_nr);

        peer.send_to(
            &UtpPacket::syn_ack(initial_syn.connection_id, 70, initial_syn.seq_nr, 65_536)
                .to_bytes(),
            client_address,
        )
        .await
        .unwrap();

        let (length, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        let request = UtpPacket::from_bytes(&buffer[..length]).unwrap();
        assert_eq!(request.packet_type().unwrap(), PacketType::StData);
        let request_handshake = Handshake::parse(&request.payload).unwrap();
        assert_eq!(request_handshake.info_hash, info_hash);
        let response = Handshake::new(&info_hash, &remote_peer_id).to_bytes();
        peer.send_to(
            &UtpPacket::data(
                initial_syn.connection_id,
                71,
                request.seq_nr,
                65_536,
                response.to_vec(),
            )
            .to_bytes(),
            client_address,
        )
        .await
        .unwrap();
    });

    let shared_socket = Arc::new(Mutex::new(UtpSocket::bind("127.0.0.1:0").unwrap()));
    shared_socket
        .lock()
        .await
        .set_connect_timeout(Duration::from_secs(5));
    let connection = tokio::time::timeout(
        Duration::from_secs(4),
        UtpPeerConnection::connect_with_shared_socket_hybrid(
            shared_socket,
            peer_address,
            &info_hash,
            None,
            &local_peer_id,
            Duration::from_secs(3),
            false,
        ),
    )
    .await
    .expect("uTP connection timers should be driven by the async wait")
    .expect("peer handshake should succeed after SYN retransmission");
    assert_eq!(connection.remote_peer_id(), Some(remote_peer_id));
    peer_task.await.unwrap();
}
