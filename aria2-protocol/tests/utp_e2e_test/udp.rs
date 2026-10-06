use super::*;

// ===========================================================================
// Section 6: Real UDP Transmission Simulation
// ===========================================================================

#[test]
fn test_utp_real_udp_syn_exchange() {
    let (server, client) = create_udp_pair();
    let server_addr = get_addr(&server);
    let client_addr = get_addr(&client);

    // Client creates SYN packet
    let syn = UtpPacket::syn(12345, 1, 0, 0);
    let syn_bytes = syn.to_bytes();

    // Send SYN to server
    assert!(send_raw(&client, &syn_bytes, server_addr));

    // Server receives SYN
    let (received, from_addr) =
        recv_with_timeout(&server, 1000).expect("Server should receive SYN");

    // Parse received packet
    let parsed = UtpPacket::from_bytes(&received).expect("Should parse SYN");
    assert_eq!(parsed.packet_type().unwrap(), PacketType::StSyn);
    assert_eq!(from_addr, client_addr);

    // Server sends SYN-ACK
    let syn_ack = UtpPacket::syn_ack(parsed.connection_id, 1, parsed.seq_nr, 65536);
    let syn_ack_bytes = syn_ack.to_bytes();

    assert!(send_raw(&server, &syn_ack_bytes, client_addr));

    // Client receives SYN-ACK
    let (received, _) = recv_with_timeout(&client, 1000).expect("Client should receive SYN-ACK");

    let parsed = UtpPacket::from_bytes(&received).expect("Should parse SYN-ACK");
    assert_eq!(parsed.packet_type().unwrap(), PacketType::StAck);
}

#[test]
fn test_utp_real_udp_data_exchange() {
    let (server, client) = create_udp_pair();
    let server_addr = get_addr(&server);
    let client_addr = get_addr(&client);

    // Simulate established connection (connection_id = 12345)
    let conn_id = 12345;

    // Client sends DATA packet
    let payload = vec![1, 2, 3, 4, 5, 6, 7, 8];
    let data = UtpPacket::data(conn_id, 2, 1, 0, payload.clone());
    let data_bytes = data.to_bytes();

    assert!(send_raw(&client, &data_bytes, server_addr));

    // Server receives DATA
    let (received, from_addr) =
        recv_with_timeout(&server, 1000).expect("Server should receive DATA");

    let parsed = UtpPacket::from_bytes(&received).expect("Should parse DATA");
    assert_eq!(parsed.packet_type().unwrap(), PacketType::StData);
    assert_eq!(parsed.payload, payload);
    assert_eq!(from_addr, client_addr);

    // Server sends ACK
    let ack = UtpPacket::ack(conn_id, 2, 1, 1024 * 1024);
    let ack_bytes = ack.to_bytes();

    assert!(send_raw(&server, &ack_bytes, client_addr));

    // Client receives ACK
    let (received, _) = recv_with_timeout(&client, 1000).expect("Client should receive ACK");

    let parsed = UtpPacket::from_bytes(&received).expect("Should parse ACK");
    assert_eq!(parsed.packet_type().unwrap(), PacketType::StAck);
    assert_eq!(parsed.ack_nr, 2);
}

#[test]
fn test_utp_real_udp_fin_exchange() {
    let (server, client) = create_udp_pair();
    let server_addr = get_addr(&server);
    let client_addr = get_addr(&client);

    let conn_id = 12345;

    // Client sends FIN to close connection
    let fin = UtpPacket::fin(conn_id, 10, 9, 0);
    let fin_bytes = fin.to_bytes();

    assert!(send_raw(&client, &fin_bytes, server_addr));

    // Server receives FIN
    let (received, _) = recv_with_timeout(&server, 1000).expect("Server should receive FIN");

    let parsed = UtpPacket::from_bytes(&received).expect("Should parse FIN");
    assert_eq!(parsed.packet_type().unwrap(), PacketType::StFin);

    // Server sends FIN-ACK
    let fin_ack = UtpPacket::fin(conn_id, 9, 10, 0);
    let fin_ack_bytes = fin_ack.to_bytes();

    assert!(send_raw(&server, &fin_ack_bytes, client_addr));

    // Client receives FIN-ACK
    let (received, _) = recv_with_timeout(&client, 1000).expect("Client should receive FIN-ACK");

    let parsed = UtpPacket::from_bytes(&received).expect("Should parse FIN-ACK");
    assert_eq!(parsed.packet_type().unwrap(), PacketType::StFin);
}

#[test]
fn test_utp_real_udp_reset() {
    let (server, client) = create_udp_pair();
    let server_addr = get_addr(&server);

    let conn_id = 12345;

    // Client sends RESET to abort connection
    let reset = UtpPacket::reset(conn_id);
    let reset_bytes = reset.to_bytes();

    assert!(send_raw(&client, &reset_bytes, server_addr));

    // Server receives RESET
    let (received, _) = recv_with_timeout(&server, 1000).expect("Server should receive RESET");

    let parsed = UtpPacket::from_bytes(&received).expect("Should parse RESET");
    assert_eq!(parsed.packet_type().unwrap(), PacketType::StReset);
}

#[test]
fn test_utp_real_udp_multiple_packets() {
    let (server, client) = create_udp_pair();
    let server_addr = get_addr(&server);

    let conn_id = 12345;

    // Send multiple DATA packets
    for i in 1..=5 {
        let payload = vec![(i % 256) as u8; 100];
        let data = UtpPacket::data(conn_id, i + 1, i, 0, payload);
        let data_bytes = data.to_bytes();

        assert!(send_raw(&client, &data_bytes, server_addr));
    }

    // Server should receive all packets
    let mut received_count = 0;
    for _ in 1..=5 {
        if recv_with_timeout(&server, 500).is_some() {
            received_count += 1;
        }
    }

    // All packets should be received
    assert_eq!(received_count, 5);
}

#[test]
fn test_utp_real_udp_sequence_numbers() {
    let (server, client) = create_udp_pair();
    let server_addr = get_addr(&server);

    let conn_id = 12345;

    // Send packets with increasing sequence numbers
    for (expected_seq, i) in (2..).zip(0..3) {
        let payload = vec![i as u8; 50];
        let data = UtpPacket::data(conn_id, expected_seq, expected_seq - 1, 0, payload);
        let data_bytes = data.to_bytes();

        assert!(send_raw(&client, &data_bytes, server_addr));

        // Receive and verify sequence number
        let (received, _) = recv_with_timeout(&server, 500).expect("Should receive packet");
        let parsed = UtpPacket::from_bytes(&received).expect("Should parse");

        assert_eq!(parsed.seq_nr, expected_seq);
    }
}

// ===========================================================================
// Section 10: Comprehensive E2E Scenario
// ===========================================================================

#[test]
fn test_utp_full_connection_lifecycle() {
    // Complete connection lifecycle: SYN -> DATA -> FIN

    let (server, client) = create_udp_pair();
    let server_addr = get_addr(&server);
    let client_addr = get_addr(&client);

    // Phase 1: Connection establishment
    let conn_id = 12345;

    // Client sends SYN
    let syn = UtpPacket::syn(conn_id, 1, 0, 0);
    send_raw(&client, &syn.to_bytes(), server_addr);

    // Server receives SYN and sends SYN-ACK
    let (syn_received, _) = recv_with_timeout(&server, 1000).expect("Server receive SYN");
    let parsed_syn = UtpPacket::from_bytes(&syn_received).expect("Parse SYN");

    let syn_ack = UtpPacket::syn_ack(parsed_syn.connection_id, 1, parsed_syn.seq_nr, 65536);
    send_raw(&server, &syn_ack.to_bytes(), client_addr);

    // Client receives SYN-ACK
    recv_with_timeout(&client, 1000).expect("Client receive SYN-ACK");

    // Phase 2: Data transfer
    let payload = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    let data = UtpPacket::data(conn_id, 2, 1, 0, payload.clone());
    send_raw(&client, &data.to_bytes(), server_addr);

    // Server receives DATA and sends ACK
    let (data_received, _) = recv_with_timeout(&server, 1000).expect("Server receive DATA");
    let parsed_data = UtpPacket::from_bytes(&data_received).expect("Parse DATA");
    assert_eq!(parsed_data.payload, payload);

    let ack = UtpPacket::ack(conn_id, 2, 1, 1024 * 1024);
    send_raw(&server, &ack.to_bytes(), client_addr);

    // Client receives ACK
    recv_with_timeout(&client, 1000).expect("Client receive ACK");

    // Phase 3: Connection teardown
    let fin = UtpPacket::fin(conn_id, 3, 2, 0);
    send_raw(&client, &fin.to_bytes(), server_addr);

    // Server receives FIN and sends FIN-ACK
    recv_with_timeout(&server, 1000).expect("Server receive FIN");

    let fin_ack = UtpPacket::fin(conn_id, 2, 3, 0);
    send_raw(&server, &fin_ack.to_bytes(), client_addr);

    // Client receives FIN-ACK
    recv_with_timeout(&client, 1000).expect("Client receive FIN-ACK");

    // Connection lifecycle complete
}

#[test]
fn test_utp_bidirectional_data_transfer() {
    // Bidirectional data transfer simulation

    let (server, client) = create_udp_pair();
    let server_addr = get_addr(&server);
    let client_addr = get_addr(&client);

    let conn_id = 12345;

    // Client -> Server: Data 1
    let data1 = vec![1, 2, 3];
    let packet1 = UtpPacket::data(conn_id, 2, 1, 0, data1.clone());
    send_raw(&client, &packet1.to_bytes(), server_addr);

    let (recv1, _) = recv_with_timeout(&server, 500).expect("Server receive data1");
    let parsed1 = UtpPacket::from_bytes(&recv1).expect("Parse");
    assert_eq!(parsed1.payload, data1);

    // Server -> Client: Data 2
    let data2 = vec![4, 5, 6];
    let packet2 = UtpPacket::data(conn_id, 2, 1, 0, data2.clone());
    send_raw(&server, &packet2.to_bytes(), client_addr);

    let (recv2, _) = recv_with_timeout(&client, 500).expect("Client receive data2");
    let parsed2 = UtpPacket::from_bytes(&recv2).expect("Parse");
    assert_eq!(parsed2.payload, data2);

    // Bidirectional transfer complete
}
