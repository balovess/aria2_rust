use super::*;
mod lifecycle;
use std::net::{IpAddr, Ipv4Addr};

fn test_addr() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12345)
}

#[test]
fn connection_id_search_wraps_and_finds_the_only_free_id() {
    assert_eq!(
        find_available_connection_id(u16::MAX, |connection_id| connection_id != 0),
        Some(0)
    );
}

#[test]
fn connection_id_search_reports_full_id_space() {
    assert_eq!(find_available_connection_id(123, |_| true), None);
}

#[tokio::test]
async fn incoming_connection_id_collision_does_not_replace_active_connection() {
    let options = UtpSocketOptions::default().with_max_connections(4);
    let mut server = UtpSocket::bind_addr_with_options(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        options,
    )
    .unwrap();
    let readiness = server.readiness_socket().unwrap();
    let first_peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let second_peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let local_connection_id = 101;
    let syn = UtpPacket::syn(local_connection_id - 1, 7, 0, 65_536).to_bytes();

    first_peer.send_to(&syn, server.local_addr()).await.unwrap();
    poll_recv_after_readable(&mut server, &readiness).await;
    let mut buffer = [0; 2048];
    let (length, _) =
        tokio::time::timeout(Duration::from_secs(2), first_peer.recv_from(&mut buffer))
            .await
            .expect("the first peer should receive SYN-ACK")
            .unwrap();
    assert_eq!(
        UtpPacket::from_bytes(&buffer[..length])
            .unwrap()
            .packet_type()
            .unwrap(),
        PacketType::StAck
    );
    assert_eq!(server.connection_count(), 1);

    second_peer
        .send_to(&syn, server.local_addr())
        .await
        .unwrap();
    poll_recv_after_readable(&mut server, &readiness).await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            second_peer.recv_from(&mut buffer)
        )
        .await
        .is_err(),
        "the colliding peer must not receive a reply that could disturb another session"
    );
    assert_eq!(server.connection_count(), 1);
    assert_eq!(
        server
            .connection_stats(local_connection_id)
            .unwrap()
            .remote_addr,
        Some(first_peer.local_addr().unwrap())
    );
}

async fn poll_recv_after_readable(socket: &mut UtpSocket, readiness: &tokio::net::UdpSocket) {
    wait_for_udp_packet(&socket.socket, readiness).await;
    socket.poll_recv().unwrap();
}

async fn wait_for_udp_packet(socket: &UdpSocket, readiness: &tokio::net::UdpSocket) {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut probe = [0; MAX_UDP_RECV_BUFFER];
        loop {
            readiness.readable().await.unwrap();
            match readiness.try_io(tokio::io::Interest::READABLE, || {
                socket.peek_from(&mut probe)
            }) {
                Ok(_) => return,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    // The socket clone may still report readiness after the
                    // protocol socket consumed the previous datagram.
                }
                Err(error) => panic!("failed to inspect queued UDP packet: {error}"),
            }
        }
    })
    .await
    .expect("peer packet should make the uTP socket readable");
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
fn socket_options_configure_connection_limit_and_timers() {
    let options = UtpSocketOptions::default()
        .with_max_connections(1)
        .with_connect_timeout(Duration::from_secs(3))
        .with_idle_timeout(Duration::from_secs(4))
        .with_keepalive_interval(Duration::from_secs(5));
    let mut socket = UtpSocket::bind_addr_with_options(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        options,
    )
    .unwrap();
    let remote = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let remote_addr = remote.local_addr().unwrap();

    assert_eq!(
        socket.timers.default_connect_timeout(),
        Duration::from_secs(3)
    );
    assert_eq!(socket.timers.default_idle_timeout(), Duration::from_secs(4));
    assert_eq!(
        socket.timers.default_keepalive_interval(),
        Duration::from_secs(5)
    );
    assert!(socket.connect(remote_addr).is_ok());
    assert!(matches!(
        socket.connect(remote_addr),
        Err(UtpSocketError::MaxConnectionsReached)
    ));
}

#[tokio::test]
async fn keepalive_sends_do_not_extend_established_idle_deadline() {
    let options = UtpSocketOptions::default()
        .with_idle_timeout(Duration::from_millis(80))
        .with_keepalive_interval(Duration::from_millis(10));
    let mut client = UtpSocket::bind_addr_with_options(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        options,
    )
    .unwrap();
    let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let conn_id = client.connect(peer.local_addr().unwrap()).unwrap();
    let readiness = client.readiness_socket().unwrap();

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
    poll_recv_after_readable(&mut client, &readiness).await;
    assert_eq!(
        client.connection_state(conn_id).unwrap(),
        ConnectionState::Established
    );

    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        client.process_timers().unwrap();
    }
    assert!(matches!(
        client.connection_state(conn_id),
        Err(UtpSocketError::ConnectionNotFound(id)) if id == conn_id
    ));
}

#[tokio::test]
async fn valid_peer_packet_refreshes_established_idle_deadline() {
    let options = UtpSocketOptions::default()
        .with_idle_timeout(Duration::from_millis(120))
        .with_keepalive_interval(Duration::from_secs(5));
    let mut client = UtpSocket::bind_addr_with_options(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        options,
    )
    .unwrap();
    let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let conn_id = client.connect(peer.local_addr().unwrap()).unwrap();
    let readiness = client.readiness_socket().unwrap();

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
    poll_recv_after_readable(&mut client, &readiness).await;

    tokio::time::sleep(Duration::from_millis(80)).await;
    peer.send_to(
        &UtpPacket::ack(syn.connection_id, syn.seq_nr, 71, 65_536).to_bytes(),
        client.local_addr(),
    )
    .await
    .unwrap();
    poll_recv_after_readable(&mut client, &readiness).await;

    // This is past the original deadline but before the refreshed deadline.
    tokio::time::sleep(Duration::from_millis(60)).await;
    client.process_timers().unwrap();
    assert_eq!(
        client.connection_state(conn_id).unwrap(),
        ConnectionState::Established
    );

    tokio::time::sleep(Duration::from_millis(70)).await;
    client.process_timers().unwrap();
    assert!(matches!(
        client.connection_state(conn_id),
        Err(UtpSocketError::ConnectionNotFound(id)) if id == conn_id
    ));
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
    wait_for_udp_packet(&client.socket, &client_readiness).await;

    let mut first_half = [0; 3];
    assert_eq!(client.recv(client_conn_id, &mut first_half).unwrap(), 3);
    assert_eq!(&first_half, b"abc");

    let mut second_half = [0; 3];
    assert_eq!(client.recv(client_conn_id, &mut second_half).unwrap(), 3);
    assert_eq!(&second_half, b"def");
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
