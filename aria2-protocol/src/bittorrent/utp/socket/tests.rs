use super::*;
use std::net::{IpAddr, Ipv4Addr};

fn test_addr() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12345)
}

#[test]
fn test_socket_bind() {
    let socket = UtpSocket::bind_any();
    assert!(socket.is_ok());

    let socket = socket.unwrap();
    assert!(socket.local_addr().port() > 0);
    assert!(!socket.is_closed());
    assert_eq!(socket.connection_count(), 0);
}

#[test]
fn test_socket_close() {
    let mut socket = UtpSocket::bind_any().unwrap();
    assert!(!socket.is_closed());

    socket.close();
    assert!(socket.is_closed());
}

#[test]
fn test_socket_connect_closed() {
    let mut socket = UtpSocket::bind_any().unwrap();
    socket.close();

    let result = socket.connect(test_addr());
    assert!(matches!(result, Err(UtpSocketError::SocketClosed)));
}

#[test]
fn test_socket_connection_not_found() {
    let mut socket = UtpSocket::bind_any().unwrap();

    let result = socket.send(60000, &[1, 2, 3]);
    assert!(matches!(
        result,
        Err(UtpSocketError::ConnectionNotFound(60000))
    ));

    let mut buf = [0u8; 100];
    let result = socket.recv(60000, &mut buf);
    assert!(matches!(
        result,
        Err(UtpSocketError::ConnectionNotFound(60000))
    ));
}

#[test]
fn test_socket_process_timers_no_connections() {
    let mut socket = UtpSocket::bind_any().unwrap();
    let result = socket.process_timers();
    assert!(result.is_ok());
}

#[test]
fn test_socket_poll_recv_no_data() {
    let mut socket = UtpSocket::bind_any().unwrap();
    let result = socket.poll_recv();
    assert!(result.is_ok());
    let data = result.unwrap();
    assert!(data.is_empty());
}

#[tokio::test]
async fn recv_preserves_unread_payload_across_small_buffers() {
    let mut client = UtpSocket::bind("127.0.0.1:0").unwrap();
    let mut server = UtpSocket::bind("127.0.0.1:0").unwrap();
    let client_conn_id = client.connect(server.local_addr()).unwrap();
    let server_readiness = server.readiness_socket().unwrap();

    tokio::time::timeout(Duration::from_secs(3), server_readiness.readable())
        .await
        .expect("server should become readable for the SYN")
        .unwrap();
    server.poll_recv().unwrap();

    let client_readiness = client.readiness_socket().unwrap();
    tokio::time::timeout(Duration::from_secs(3), client_readiness.readable())
        .await
        .expect("client should become readable for the SYN-ACK")
        .unwrap();
    client.poll_recv().unwrap();
    assert_eq!(
        client.connection_state(client_conn_id).unwrap(),
        ConnectionState::Established
    );

    let server_conn_id = server.connection_ids()[0];
    assert_eq!(server.send(server_conn_id, b"abcdef").unwrap(), 6);
    tokio::time::timeout(Duration::from_secs(3), client_readiness.readable())
        .await
        .expect("client should become readable for the data")
        .unwrap();

    let mut first_half = [0; 3];
    assert_eq!(client.recv(client_conn_id, &mut first_half).unwrap(), 3);
    assert_eq!(&first_half, b"abc");

    let mut second_half = [0; 3];
    assert_eq!(client.recv(client_conn_id, &mut second_half).unwrap(), 3);
    assert_eq!(&second_half, b"def");
}

#[tokio::test]
async fn retransmit_timer_sends_only_its_packet_and_preserves_backoff() {
    let mut client = UtpSocket::bind("127.0.0.1:0").unwrap();
    let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let conn_id = client.connect(peer.local_addr().unwrap()).unwrap();

    let mut wire_buffer = [0; 2048];
    let (length, _) =
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut wire_buffer))
            .await
            .unwrap()
            .unwrap();
    let syn = UtpPacket::from_bytes(&wire_buffer[..length]).unwrap();
    peer.send_to(
        &UtpPacket::syn_ack(syn.connection_id, 70, syn.seq_nr, 65_536).to_bytes(),
        client.local_addr(),
    )
    .await
    .unwrap();
    client.poll_recv().unwrap();
    assert_eq!(
        client.connection_state(conn_id).unwrap(),
        ConnectionState::Established
    );

    assert_eq!(client.send(conn_id, &vec![0x5a; 2800]).unwrap(), 2800);
    let (first_len, _) = peer.recv_from(&mut wire_buffer).await.unwrap();
    let first = UtpPacket::from_bytes(&wire_buffer[..first_len]).unwrap();
    let (second_len, _) = peer.recv_from(&mut wire_buffer).await.unwrap();
    let second = UtpPacket::from_bytes(&wire_buffer[..second_len]).unwrap();
    assert_ne!(first.seq_nr, second.seq_nr);

    client
        .timers
        .set_timer(conn_id, TimerType::Retransmit(first.seq_nr), Duration::ZERO);
    client.process_timers().unwrap();

    let (retry_len, _) =
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut wire_buffer))
            .await
            .unwrap()
            .unwrap();
    let retry = UtpPacket::from_bytes(&wire_buffer[..retry_len]).unwrap();
    assert_eq!(retry.seq_nr, first.seq_nr);
    assert_eq!(retry.payload, first.payload);
    assert_eq!(
        client.timers.retransmit_count(conn_id, first.seq_nr),
        Some(1)
    );

    assert!(
        tokio::time::timeout(Duration::from_millis(100), peer.recv_from(&mut wire_buffer))
            .await
            .is_err()
    );
    assert!(
        client
            .timers
            .has_timer(conn_id, TimerType::Retransmit(second.seq_nr))
    );

    peer.send_to(
        &UtpPacket::ack(syn.connection_id, first.seq_nr, 71, 65_536).to_bytes(),
        client.local_addr(),
    )
    .await
    .unwrap();
    client.poll_recv().unwrap();
    assert!(
        !client
            .timers
            .has_timer(conn_id, TimerType::Retransmit(first.seq_nr))
    );
    assert!(
        client
            .timers
            .has_timer(conn_id, TimerType::Retransmit(second.seq_nr))
    );
}

#[tokio::test]
async fn close_connection_retransmits_fin_until_acknowledged() {
    let mut client = UtpSocket::bind("127.0.0.1:0").unwrap();
    let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let conn_id = client.connect(peer.local_addr().unwrap()).unwrap();

    let mut wire_buffer = [0; 2048];
    let (syn_len, _) =
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut wire_buffer))
            .await
            .unwrap()
            .unwrap();
    let syn = UtpPacket::from_bytes(&wire_buffer[..syn_len]).unwrap();
    peer.send_to(
        &UtpPacket::syn_ack(syn.connection_id, 70, syn.seq_nr, 65_536).to_bytes(),
        client.local_addr(),
    )
    .await
    .unwrap();
    client.poll_recv().unwrap();
    assert_eq!(
        client.connection_state(conn_id).unwrap(),
        ConnectionState::Established
    );

    client.close_connection(conn_id).unwrap();
    let (fin_len, _) =
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut wire_buffer))
            .await
            .unwrap()
            .unwrap();
    let fin = UtpPacket::from_bytes(&wire_buffer[..fin_len]).unwrap();
    assert_eq!(fin.packet_type().unwrap(), PacketType::StFin);
    assert_eq!(
        client.connection_state(conn_id).unwrap(),
        ConnectionState::FinWait
    );

    let retry_delay = client
        .next_timer_delay()
        .expect("unacknowledged FIN must retain a retransmission deadline");
    tokio::time::sleep(retry_delay + Duration::from_millis(10)).await;
    client.process_timers().unwrap();

    let (retry_len, _) =
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut wire_buffer))
            .await
            .unwrap()
            .unwrap();
    let retry = UtpPacket::from_bytes(&wire_buffer[..retry_len]).unwrap();
    assert_eq!(retry.packet_type().unwrap(), PacketType::StFin);
    assert_eq!(retry.connection_id, fin.connection_id);
    assert_eq!(retry.seq_nr, fin.seq_nr);

    peer.send_to(
        &UtpPacket::ack(syn.connection_id, fin.seq_nr, 71, 65_536).to_bytes(),
        client.local_addr(),
    )
    .await
    .unwrap();
    client.poll_recv().unwrap();
    assert!(matches!(
        client.connection_state(conn_id),
        Err(UtpSocketError::ConnectionNotFound(id)) if id == conn_id
    ));
}

#[tokio::test]
async fn received_fin_is_acknowledged_again_and_retired_after_idle_deadline() {
    let mut client = UtpSocket::bind("127.0.0.1:0").unwrap();
    client.set_idle_timeout(Duration::from_millis(40));
    let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let conn_id = client.connect(peer.local_addr().unwrap()).unwrap();

    let mut wire_buffer = [0; 2048];
    let (syn_len, _) =
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut wire_buffer))
            .await
            .unwrap()
            .unwrap();
    let syn = UtpPacket::from_bytes(&wire_buffer[..syn_len]).unwrap();
    peer.send_to(
        &UtpPacket::syn_ack(syn.connection_id, 70, syn.seq_nr, 65_536).to_bytes(),
        client.local_addr(),
    )
    .await
    .unwrap();
    client.poll_recv().unwrap();

    let fin = UtpPacket::fin(syn.connection_id, 71, syn.seq_nr, 65_536);
    peer.send_to(&fin.to_bytes(), client.local_addr())
        .await
        .unwrap();
    client.poll_recv().unwrap();
    let (ack_len, _) =
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut wire_buffer))
            .await
            .unwrap()
            .unwrap();
    let ack = UtpPacket::from_bytes(&wire_buffer[..ack_len]).unwrap();
    assert_eq!(ack.packet_type().unwrap(), PacketType::StAck);
    assert_eq!(ack.ack_nr, fin.seq_nr);
    assert_eq!(
        client.connection_state(conn_id).unwrap(),
        ConnectionState::Closing
    );

    peer.send_to(&fin.to_bytes(), client.local_addr())
        .await
        .unwrap();
    client.poll_recv().unwrap();
    let (duplicate_ack_len, _) =
        tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut wire_buffer))
            .await
            .unwrap()
            .unwrap();
    let duplicate_ack = UtpPacket::from_bytes(&wire_buffer[..duplicate_ack_len]).unwrap();
    assert_eq!(duplicate_ack.packet_type().unwrap(), PacketType::StAck);
    assert_eq!(duplicate_ack.ack_nr, fin.seq_nr);

    let idle_deadline = client
        .next_timer_delay()
        .expect("closing connection must have a bounded retirement deadline");
    tokio::time::sleep(idle_deadline + Duration::from_millis(5)).await;
    client.process_timers().unwrap();
    assert!(matches!(
        client.connection_state(conn_id),
        Err(UtpSocketError::ConnectionNotFound(id)) if id == conn_id
    ));
}

#[test]
fn test_socket_routes_syn_ack_to_outgoing_connection() {
    let mut client = UtpSocket::bind("127.0.0.1:0").unwrap();
    let mut server = UtpSocket::bind("127.0.0.1:0").unwrap();
    let connection_id = client.connect(server.local_addr()).unwrap();

    for _ in 0..200 {
        server.poll_recv().unwrap();
        client.poll_recv().unwrap();
        if client.connection_state(connection_id).unwrap() == ConnectionState::Established {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    assert_eq!(
        client.connection_state(connection_id).unwrap(),
        ConnectionState::Established
    );
    assert_eq!(server.connection_count(), 1);
    let server_connection_id = server.connection_ids()[0];
    assert_eq!(
        server.connection_state(server_connection_id).unwrap(),
        ConnectionState::Established
    );
}
