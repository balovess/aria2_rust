use super::*;

// ===========================================================================
// Section 2: Connection State Machine Tests
// ===========================================================================

#[test]
fn test_utp_connection_initial_state() {
    let conn = UtpConnection::new();

    assert_eq!(conn.state(), ConnectionState::Closed);
    assert!(!conn.is_established());
}

#[test]
fn test_utp_connection_connect_transition() {
    let mut conn = UtpConnection::new();

    // Initiate connection
    let result = conn.connect(std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        12345,
    ));

    assert!(result.is_ok());
    assert_eq!(conn.state(), ConnectionState::SynSent);
}

#[test]
fn test_utp_connection_accept_syn() {
    let mut server_conn = UtpConnection::new();

    // Create SYN packet from client
    let syn = UtpPacket::syn(12345, 1, 0, 0);

    let client_addr = std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        54321,
    );

    // Server accepts SYN
    let result = server_conn.accept(&syn, client_addr);

    assert!(result.is_ok());
    assert_eq!(server_conn.state(), ConnectionState::Established);
    assert!(server_conn.is_established());
}

#[test]
fn test_utp_connection_full_handshake() {
    // Simulate full SYN -> SYN-ACK -> ACK handshake

    // Client initiates
    let mut client_conn = UtpConnection::new();
    client_conn
        .connect(std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
            12345,
        ))
        .expect("Client connect failed");

    assert_eq!(client_conn.state(), ConnectionState::SynSent);

    // Server accepts
    let mut server_conn = UtpConnection::new();
    let syn = UtpPacket::syn(client_conn.local_connection_id(), 1, 0, 0);

    let syn_ack = server_conn
        .accept(
            &syn,
            std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
                54321,
            ),
        )
        .expect("Server accept failed");

    assert_eq!(server_conn.state(), ConnectionState::Established);

    // Client handles the response
    client_conn
        .on_packet_received(&syn_ack)
        .expect("Client handle SYN-ACK failed");

    assert_eq!(client_conn.state(), ConnectionState::Established);
}

#[test]
fn test_utp_connection_graceful_close() {
    let mut conn = UtpConnection::new();

    // First establish connection
    conn.connect(std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        12345,
    ))
    .expect("Connect failed");

    // Simulate SYN-ACK received
    let syn_ack = UtpPacket::syn_ack(conn.local_connection_id(), 0, 1, 65536);
    conn.on_packet_received(&syn_ack)
        .expect("Handle SYN-ACK failed");

    assert!(conn.is_established());

    // Now close gracefully
    conn.close().expect("Close failed");

    assert_eq!(conn.state(), ConnectionState::FinWait);
}

#[test]
fn test_utp_connection_reset() {
    let mut conn = UtpConnection::new();

    conn.connect(std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        12345,
    ))
    .expect("Connect failed");

    // Force reset — simulate receiving a RESET packet
    let reset = UtpPacket::reset(conn.local_connection_id());
    let _ = conn.on_packet_received(&reset);

    assert_eq!(conn.state(), ConnectionState::Closed);
}

#[test]
fn test_utp_connection_handle_reset_packet() {
    let mut conn = UtpConnection::new();

    // Establish connection
    conn.connect(std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        12345,
    ))
    .expect("Connect failed");

    let syn_ack = UtpPacket::syn_ack(conn.local_connection_id(), 0, 1, 65536);
    conn.on_packet_received(&syn_ack)
        .expect("Handle SYN-ACK failed");

    assert!(conn.is_established());

    // Receive RESET from peer - this should return ConnectionReset error
    let reset = UtpPacket::reset(conn.local_connection_id());
    let result = conn.on_packet_received(&reset);

    // RESET should cause ConnectionReset error and close the connection
    assert!(result.is_err());
    assert_eq!(conn.state(), ConnectionState::Closed);
}

// ===========================================================================
// Section 3: Data Transfer Tests
// ===========================================================================

#[test]
fn test_utp_connection_send_data() {
    let mut conn = UtpConnection::new();

    // Establish connection
    conn.connect(std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        12345,
    ))
    .expect("Connect failed");

    let syn_ack = UtpPacket::syn_ack(conn.local_connection_id(), 0, 1, 65536);
    conn.on_packet_received(&syn_ack)
        .expect("Handle SYN-ACK failed");

    // Send data
    let data = vec![1, 2, 3, 4, 5];
    conn.send_data(&data).expect("Send data failed");

    // Verify sequence number incremented
    assert!(conn.current_seq_nr() > 1);
}

#[test]
fn test_utp_connection_receive_data() {
    let mut conn = UtpConnection::new();

    // Establish connection
    conn.connect(std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        12345,
    ))
    .expect("Connect failed");

    let syn_ack = UtpPacket::syn_ack(conn.local_connection_id(), 0, 1, 65536);
    conn.on_packet_received(&syn_ack)
        .expect("Handle SYN-ACK failed");

    // Receive DATA packet - seq_nr must match expected_recv_seq (1)
    let payload = vec![10, 20, 30, 40, 50];
    let data_packet = UtpPacket::data(conn.local_connection_id(), 1, 1, 0, payload.clone());

    conn.on_packet_received(&data_packet)
        .expect("Handle DATA failed");

    // Verify data received
    let received = conn.recv_data();
    assert_eq!(received, payload);
}

#[test]
fn test_utp_connection_ack_handling() {
    let mut conn = UtpConnection::new();

    // Establish connection
    conn.connect(std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        12345,
    ))
    .expect("Connect failed");

    let syn_ack = UtpPacket::syn_ack(conn.local_connection_id(), 0, 1, 65536);
    conn.on_packet_received(&syn_ack)
        .expect("Handle SYN-ACK failed");

    // Send some data
    conn.send_data(&[1, 2, 3]).expect("Send failed");
    conn.send_data(&[4, 5, 6]).expect("Send failed");

    // Receive ACK for first packet
    let ack = UtpPacket::ack(conn.local_connection_id(), 2, 1, 1024 * 1024);
    conn.on_packet_received(&ack).expect("Handle ACK failed");

    // Verify congestion window updated
    assert!(conn.congestion_window() > 0);
}

// ===========================================================================
// Section 9: Integration with BitTorrent Context
// ===========================================================================

#[test]
fn test_utp_packet_bit_torrent_context() {
    // Simulate BitTorrent piece data transfer via uTP

    // Create mock piece data
    let piece_data: Vec<u8> = (0..16384).map(|i| (i % 256) as u8).collect();

    // Split into multiple uTP packets
    let chunk_size = 1000;
    let conn_id = 12345;

    for (seq, chunk) in (2..).zip(piece_data.chunks(chunk_size)) {
        let packet = UtpPacket::data(conn_id, seq, seq - 1, 0, chunk.to_vec());

        // Verify packet creation
        let bytes = packet.to_bytes();
        assert!(bytes.len() > 20);
    }
}

#[test]
fn test_utp_connection_bit_torrent_handshake() {
    // Simulate BitTorrent protocol handshake over uTP

    let mut client_conn = UtpConnection::new();

    // Establish uTP connection
    client_conn
        .connect(std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
            12345,
        ))
        .expect("Connect failed");

    // Simulate SYN-ACK
    let syn_ack = UtpPacket::syn_ack(client_conn.local_connection_id(), 0, 1, 65536);
    client_conn
        .on_packet_received(&syn_ack)
        .expect("Handle SYN-ACK failed");

    assert!(client_conn.is_established());

    // Send BitTorrent handshake (68 bytes)
    let bt_handshake: Vec<u8> = vec![
        19, // Protocol name length
        b'B', b'i', b't', b'T', b'o', b'r', b'r', b'e', b'n', b't', b' ', b'p', b'r', b'o', b't',
        b'o', b'c', b'o', b'l', // "BitTorrent protocol"
        0, 0, 0, 0, 0, 0, 0, 0, // Reserved bytes
        // Info hash (20 bytes)
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
        // Peer ID (20 bytes)
        b'A', b'R', b'I', b'A', b'2', b'R', b'S', b'T', b'0', b'0', b'0', b'0', b'0', b'0', b'0',
        b'0', b'0', b'0', b'0', b'0',
    ];

    client_conn
        .send_data(&bt_handshake)
        .expect("Send BT handshake failed");

    // Verify data was queued
    assert!(client_conn.current_seq_nr() > 1);
}

#[test]
fn test_utp_connection_bit_torrent_piece_request() {
    // Simulate BitTorrent piece request over uTP

    let mut conn = UtpConnection::new();

    // Establish connection
    conn.connect(std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        12345,
    ))
    .expect("Connect failed");

    let syn_ack = UtpPacket::syn_ack(conn.local_connection_id(), 0, 1, 65536);
    conn.on_packet_received(&syn_ack)
        .expect("Handle SYN-ACK failed");

    // Send piece request (ID=6, index=0, begin=0, length=16384)
    let piece_request: Vec<u8> = vec![
        0, 0, 0, 13, // Length prefix (13 bytes)
        6,  // Message ID (request)
        0, 0, 0, 0, // Piece index
        0, 0, 0, 0, // Begin offset
        0, 0, 64, 0, // Length (16384)
    ];

    conn.send_data(&piece_request)
        .expect("Send piece request failed");
}

#[test]
fn test_utp_connection_bit_torrent_piece_data() {
    // Simulate receiving BitTorrent piece data over uTP

    let mut conn = UtpConnection::new();

    // Establish connection
    conn.connect(std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        12345,
    ))
    .expect("Connect failed");

    let syn_ack = UtpPacket::syn_ack(conn.local_connection_id(), 0, 1, 65536);
    conn.on_packet_received(&syn_ack)
        .expect("Handle SYN-ACK failed");

    // Receive piece data (ID=7)
    let piece_data_header: Vec<u8> = vec![
        0, 0, 64, 21, // Length prefix (16389 bytes = 9 + 16384)
        7,  // Message ID (piece)
        0, 0, 0, 0, // Piece index
        0, 0, 0, 0, // Begin offset
    ];

    // Simulate receiving header - seq_nr must match expected_recv_seq (1)
    let data_packet = UtpPacket::data(
        conn.local_connection_id(),
        1,
        1,
        0,
        piece_data_header.clone(),
    );

    conn.on_packet_received(&data_packet)
        .expect("Handle piece header failed");

    // Verify data received
    let received = conn.recv_data();
    assert!(!received.is_empty());
}
