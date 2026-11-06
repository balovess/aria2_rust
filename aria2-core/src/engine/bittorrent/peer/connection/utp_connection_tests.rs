use super::super::utp_transport::{UtpTransportActor, UtpTransportHandle};
use super::utp_connection::UtpPeerConnection;
use aria2_protocol::bittorrent::utp::UtpSocket;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn start_transport() -> (UtpTransportHandle, UtpTransportActor, CancellationToken) {
    let (incoming_tx, _incoming_rx) = mpsc::channel(8);
    let shutdown = CancellationToken::new();
    let actor = UtpTransportActor::bind(
        "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        incoming_tx,
        shutdown.clone(),
    )
    .expect("uTP transport actor should bind");
    let transport = actor.handle();
    (transport, actor, shutdown)
}

#[tokio::test]
async fn actor_backed_connection_preserves_fragmented_bittorrent_frames() {
    use aria2_protocol::bittorrent::message::handshake::Handshake;

    let info_hash = [7u8; 20];
    let local_peer_id = [8u8; 20];
    let remote_peer_id = [9u8; 20];
    let (transport, actor, shutdown) = start_transport();
    let mut server = UtpSocket::bind("127.0.0.1:0").unwrap();
    let server_addr = server.local_addr();

    let server_task = tokio::spawn(async move {
        let mut request_buffer = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            for (conn_id, data) in server.poll_recv().unwrap() {
                request_buffer.extend_from_slice(&data);
                if request_buffer.len() >= 68 {
                    let request = Handshake::parse(&request_buffer[..68]).unwrap();
                    assert_eq!(request.info_hash, info_hash);
                    server
                        .send(
                            conn_id,
                            &Handshake::new(&info_hash, &remote_peer_id).to_bytes(),
                        )
                        .unwrap();
                    server.send(conn_id, &[0, 0, 0, 1]).unwrap();
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    server.send(conn_id, &[0]).unwrap();
                    return;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "BT handshake not received"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });

    let mut connection = UtpPeerConnection::connect_with_transport_hybrid(
        &transport,
        server_addr,
        &info_hash,
        None,
        &local_peer_id,
        Duration::from_secs(2),
        false,
    )
    .await
    .expect("uTP connection should complete the BitTorrent handshake");
    assert_eq!(connection.remote_peer_id(), Some(remote_peer_id));
    let frame = tokio::time::timeout(Duration::from_secs(1), connection.recv_message())
        .await
        .expect("fragmented uTP frame should complete")
        .expect("frame receive should succeed");
    assert_eq!(frame, Some(vec![0, 0, 0, 1, 0]));
    server_task.await.unwrap();
    shutdown.cancel();
    actor.join().await.unwrap();
}

#[tokio::test]
async fn actor_backed_connection_accepts_bep52_v2_response_over_utp() {
    use aria2_protocol::bittorrent::message::handshake::Handshake;

    let info_hash_v1 = [17u8; 20];
    let info_hash_v2 = [18u8; 32];
    let local_peer_id = [19u8; 20];
    let remote_peer_id = [20u8; 20];
    let (transport, actor, shutdown) = start_transport();
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
                let request = Handshake::parse(&request_buffer[..68]).unwrap();
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

    let connection = UtpPeerConnection::connect_with_transport_hybrid(
        &transport,
        server_addr,
        &info_hash_v1,
        Some(&info_hash_v2),
        &local_peer_id,
        Duration::from_secs(2),
        false,
    )
    .await
    .unwrap();
    server_task.await.unwrap();
    assert_eq!(connection.remote_peer_id(), Some(remote_peer_id));
    shutdown.cancel();
    actor.join().await.unwrap();
}

#[tokio::test]
async fn actor_drives_syn_retransmission_without_peer_owned_socket_polling() {
    use aria2_protocol::bittorrent::message::handshake::Handshake;
    use aria2_protocol::bittorrent::utp::{PacketType, UtpPacket};

    let info_hash = [27u8; 20];
    let local_peer_id = [28u8; 20];
    let remote_peer_id = [29u8; 20];
    let (transport, actor, shutdown) = start_transport();
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
                .expect("actor must drive SYN retransmission")
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

    let connection = tokio::time::timeout(
        Duration::from_secs(4),
        UtpPeerConnection::connect_with_transport_hybrid(
            &transport,
            peer_address,
            &info_hash,
            None,
            &local_peer_id,
            Duration::from_secs(3),
            false,
        ),
    )
    .await
    .expect("uTP actor should keep timers progressing")
    .expect("peer handshake should succeed after SYN retransmission");
    assert_eq!(connection.remote_peer_id(), Some(remote_peer_id));
    peer_task.await.unwrap();
    shutdown.cancel();
    actor.join().await.unwrap();
}
