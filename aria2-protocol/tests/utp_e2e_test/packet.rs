use super::*;

// ===========================================================================
// Section 1: Packet Serialization Tests
// ===========================================================================

#[test]
fn test_utp_packet_syn_serialization() {
    // Create SYN packet for connection initiation
    let syn = UtpPacket::syn(12345, 1, 0, 0);

    // Serialize to bytes
    let bytes = syn.to_bytes();

    // Verify header size (20 bytes per BEP 29)
    assert_eq!(bytes.len(), 20);

    // Deserialize back
    let parsed = UtpPacket::from_bytes(&bytes).expect("Failed to parse SYN packet");

    // Verify all fields match
    assert_eq!(parsed.packet_type().unwrap(), PacketType::StSyn);
    assert_eq!(parsed.connection_id, 12345);
    assert_eq!(parsed.seq_nr, 1);
}

#[test]
fn test_utp_packet_data_serialization() {
    // Create DATA packet with payload
    let payload = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    let data = UtpPacket::data(12345, 2, 1, 0, payload.clone());

    // Serialize
    let bytes = data.to_bytes();

    // Verify size = header + payload
    assert_eq!(bytes.len(), 20 + payload.len());

    // Deserialize
    let parsed = UtpPacket::from_bytes(&bytes).expect("Failed to parse DATA packet");

    assert_eq!(parsed.packet_type().unwrap(), PacketType::StData);
    assert_eq!(parsed.seq_nr, 2);
    assert_eq!(parsed.ack_nr, 1);
    assert_eq!(parsed.payload, payload);
}

#[test]
fn test_utp_packet_ack_serialization() {
    // Create ACK packet
    let ack = UtpPacket::ack(12345, 2, 1, 256 * 1024);

    let bytes = ack.to_bytes();
    assert_eq!(bytes.len(), 20);

    let parsed = UtpPacket::from_bytes(&bytes).expect("Failed to parse ACK packet");

    assert_eq!(parsed.packet_type().unwrap(), PacketType::StAck);
    assert_eq!(parsed.seq_nr, 1);
    assert_eq!(parsed.ack_nr, 2);
    assert_eq!(parsed.wnd_size, 256 * 1024);
}

#[test]
fn test_utp_packet_fin_serialization() {
    // Create FIN packet for graceful close
    let fin = UtpPacket::fin(12345, 10, 9, 0);

    let bytes = fin.to_bytes();
    assert_eq!(bytes.len(), 20);

    let parsed = UtpPacket::from_bytes(&bytes).expect("Failed to parse FIN packet");

    assert_eq!(parsed.packet_type().unwrap(), PacketType::StFin);
    assert_eq!(parsed.seq_nr, 10);
    assert_eq!(parsed.ack_nr, 9);
}

#[test]
fn test_utp_packet_reset_serialization() {
    // Create RESET packet for abort
    let reset = UtpPacket::reset(12345);

    let bytes = reset.to_bytes();
    assert_eq!(bytes.len(), 20);

    let parsed = UtpPacket::from_bytes(&bytes).expect("Failed to parse RESET packet");

    assert_eq!(parsed.packet_type().unwrap(), PacketType::StReset);
}

// ===========================================================================
// Section 7: Error Handling Tests
// ===========================================================================

#[test]
fn test_utp_invalid_packet_handling() {
    // Try to parse garbage data
    let garbage = vec![0xFF, 0xFE, 0xFD, 0xFC];
    let result = UtpPacket::from_bytes(&garbage);

    // Should fail to parse
    assert!(result.is_err());
}

#[test]
fn test_utp_truncated_packet_handling() {
    // Create valid packet then truncate
    let syn = UtpPacket::syn(12345, 1, 0, 0);
    let bytes = syn.to_bytes();

    // Truncate to less than header size
    let truncated = &bytes[..10];
    let result = UtpPacket::from_bytes(truncated);

    // Should fail
    assert!(result.is_err());
}

#[test]
fn test_utp_connection_invalid_state_transition() {
    let mut conn = UtpConnection::new();

    // Try to send data without connection
    let result = conn.send_data(&[1, 2, 3]);

    // Should fail (not connected)
    assert!(result.is_err());
}

#[test]
fn test_utp_connection_double_connect() {
    let mut conn = UtpConnection::new();

    // First connect succeeds
    conn.connect(std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        12345,
    ))
    .expect("First connect should succeed");

    // Second connect should fail (already in SynSent)
    let result = conn.connect(std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        54321,
    ));

    assert!(result.is_err());
}

#[test]
fn test_utp_connection_close_not_connected() {
    let mut conn = UtpConnection::new();

    // Try to close without connection
    let result = conn.close();

    // Should fail (not connected)
    assert!(result.is_err());
}

#[test]
fn test_utp_socket_bind_any() {
    let socket = UtpSocket::bind_any().expect("Should bind to any port");

    // Should have valid local address
    let addr = socket.local_addr();
    assert!(addr.port() > 0);
}
