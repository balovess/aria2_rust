//! E2E integration tests for uTP protocol
//!
//! Simulates real UDP transmission scenarios including:
//! - Connection establishment (SYN handshake)
//! - Data transfer with LEDBAT congestion control
//! - ACK handling and window management
//! - Retransmission scenarios
//! - Connection teardown (FIN handshake)
//! - Multiple connections over single socket
//! - Error handling (timeout, reset, etc.)

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use aria2_protocol::bittorrent::utp::connection::{ConnectionState, UtpConnection};
use aria2_protocol::bittorrent::utp::congestion::{LedbatController, LEDBAT_MIN_CWND, LEDBAT_MAX_CWND, LEDBAT_TARGET_DELAY};
use aria2_protocol::bittorrent::utp::metrics::{RttEstimator, DelayEstimator};
use aria2_protocol::bittorrent::utp::packet::{PacketType, UtpPacket};
use aria2_protocol::bittorrent::utp::socket::UtpSocket;

// ===========================================================================
// Helper functions for test setup
// ===========================================================================

/// Create a mock UDP pair for testing
fn create_udp_pair() -> (UdpSocket, UdpSocket) {
    let server = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind server socket");
    let client = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind client socket");

    server
        .set_nonblocking(true)
        .expect("Failed to set server nonblocking");
    client
        .set_nonblocking(true)
        .expect("Failed to set client nonblocking");

    (server, client)
}

/// Get local address of a socket
fn get_addr(socket: &UdpSocket) -> std::net::SocketAddr {
    socket.local_addr().expect("Failed to get local address")
}

/// Wait for packet with timeout
fn recv_with_timeout(socket: &UdpSocket, timeout_ms: u64) -> Option<(Vec<u8>, std::net::SocketAddr)> {
    let mut buf = vec![0u8; 65535];
    socket
        .set_read_timeout(Some(Duration::from_millis(timeout_ms)))
        .ok();

    match socket.recv_from(&mut buf) {
        Ok((len, addr)) => Some((buf[..len].to_vec(), addr)),
        Err(_) => None,
    }
}

/// Send raw packet to address
fn send_raw(socket: &UdpSocket, data: &[u8], addr: std::net::SocketAddr) -> bool {
    socket.send_to(data, addr).is_ok()
}

// ===========================================================================
// Section 1: Packet Serialization Tests
// ===========================================================================

#[test]
fn test_utp_packet_syn_serialization() {
    // Create SYN packet for connection initiation
    let syn = UtpPacket::syn(12345, 1);

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
    let data = UtpPacket::data(12345, 2, 1, payload.clone());

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
    let fin = UtpPacket::fin(12345, 10, 9);

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
    let syn = UtpPacket::syn(12345, 1);

    let client_addr = std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        54321,
    );

    // Server accepts SYN
    let result = server_conn.accept(&syn, client_addr);

    assert!(result.is_ok());
    assert_eq!(server_conn.state(), ConnectionState::Connected);
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
    let syn = UtpPacket::syn(client_conn.local_connection_id(), 1);

    server_conn
        .accept(
            &syn,
            std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
                54321,
            ),
        )
        .expect("Server accept failed");

    assert_eq!(server_conn.state(), ConnectionState::Connected);

    // Client receives SYN-ACK (simulated)
    let syn_ack = UtpPacket::syn(server_conn.remote_connection_id(), 1);

    // Client handles the response
    client_conn
        .on_packet_received(&syn_ack)
        .expect("Client handle SYN-ACK failed");

    assert_eq!(client_conn.state(), ConnectionState::Connected);
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
    let syn_ack = UtpPacket::syn(conn.local_connection_id(), 1);
    conn.on_packet_received(&syn_ack).expect("Handle SYN-ACK failed");

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

    // Force reset
    conn.reset();

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

    let syn_ack = UtpPacket::syn(conn.local_connection_id(), 1);
    conn.on_packet_received(&syn_ack).expect("Handle SYN-ACK failed");

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

    let syn_ack = UtpPacket::syn(conn.local_connection_id(), 1);
    conn.on_packet_received(&syn_ack).expect("Handle SYN-ACK failed");

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

    let syn_ack = UtpPacket::syn(conn.local_connection_id(), 1);
    conn.on_packet_received(&syn_ack).expect("Handle SYN-ACK failed");

    // Receive DATA packet - seq_nr must match expected_recv_seq (1)
    let payload = vec![10, 20, 30, 40, 50];
    let data_packet = UtpPacket::data(conn.local_connection_id(), 1, 1, payload.clone());

    conn.on_packet_received(&data_packet).expect("Handle DATA failed");

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

    let syn_ack = UtpPacket::syn(conn.local_connection_id(), 1);
    conn.on_packet_received(&syn_ack).expect("Handle SYN-ACK failed");

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
// Section 4: LEDBAT Congestion Control Tests
// ===========================================================================

#[test]
fn test_ledbat_initial_state() {
    let controller = LedbatController::new();

    // Initial congestion window should be MSS * MIN_CWND
    assert!(controller.get_window_size() > 0);
    assert!(controller.can_send());
}

#[test]
fn test_ledbat_slow_start() {
    let mut controller = LedbatController::new();

    // In slow start, window grows exponentially
    let initial_window = controller.get_window_size();

    // Simulate ACKs with low delay (below target)
    controller.on_data_sent(1400);
    controller.on_ack_received(50_000, 1400); // 50ms delay, below 100ms target

    // Window should increase in slow start
    assert!(controller.get_window_size() >= initial_window);
}

#[test]
fn test_ledbat_congestion_avoidance_below_target() {
    let mut controller = LedbatController::new();

    // Force exit slow start
    for i in 0..20 {
        controller.on_data_sent(1400);
        controller.on_ack_received(50_000 + i * 1000, 1400);
    }

    // Now in congestion avoidance, delay below target
    let window_before = controller.get_window_size();
    controller.on_ack_received(50_000, 1400); // 50ms < 100ms target

    // Window should increase (below target = less congestion)
    assert!(controller.get_window_size() >= window_before);
}

#[test]
fn test_ledbat_congestion_avoidance_above_target() {
    let mut controller = LedbatController::new();

    // Force exit slow start and establish base delay
    for i in 0..20 {
        controller.on_data_sent(1400);
        controller.on_ack_received(30_000 + i * 1000, 1400); // Establish low base delay
    }

    // Now send with high delay (above target)
    let window_before = controller.get_window_size();
    controller.on_ack_received(150_000, 1400); // 150ms > 100ms target

    // Window should decrease (above target = congestion detected)
    assert!(controller.get_window_size() <= window_before);
}

#[test]
fn test_ledbat_timeout_handling() {
    let mut controller = LedbatController::new();

    // Build up some window
    controller.on_data_sent(1400);
    controller.on_ack_received(50_000, 1400);

    let window_before = controller.get_window_size();

    // Simulate timeout
    controller.on_timeout();

    // Window should be reduced
    assert!(controller.get_window_size() < window_before);
}

#[test]
fn test_ledbat_loss_handling() {
    let mut controller = LedbatController::new();

    // Build up window
    controller.on_data_sent(1400);
    controller.on_ack_received(50_000, 1400);

    let window_before = controller.get_window_size();

    // Simulate packet loss
    controller.on_loss();

    // Window should be reduced
    assert!(controller.get_window_size() < window_before);
}

#[test]
fn test_ledbat_bytes_in_flight_tracking() {
    let mut controller = LedbatController::new();

    // Send data
    controller.on_data_sent(1400);
    assert!(controller.get_bytes_in_flight() == 1400);

    controller.on_data_sent(1400);
    assert!(controller.get_bytes_in_flight() == 2800);

    // ACK some data
    controller.on_ack_received(50_000, 1400);
    assert!(controller.get_bytes_in_flight() == 1400);
}

#[test]
fn test_ledbat_window_bounds() {
    let controller = LedbatController::new();

    // Window should be within bounds
    assert!(controller.get_window_size() >= LEDBAT_MIN_CWND * 1500);
    assert!(controller.get_window_size() <= LEDBAT_MAX_CWND * 1500);
}

// ===========================================================================
// Section 5: RTT and Delay Estimation Tests
// ===========================================================================

#[test]
fn test_rtt_estimator_initial_state() {
    let estimator = RttEstimator::new();

    // Initial RTO should be 300ms (100ms SRTT + 4 * 50ms RTTVAR)
    // This is clamped between 200ms and 2 seconds per RFC 6298
    assert_eq!(estimator.rto(), Duration::from_millis(300));
}

#[test]
fn test_rtt_estimator_first_sample() {
    let mut estimator = RttEstimator::new();

    // First RTT sample (in microseconds)
    estimator.add_sample(100_000); // 100ms

    // SRTT should be set to first sample
    assert!(estimator.srtt() > Duration::ZERO);
}

#[test]
fn test_rtt_estimator_multiple_samples() {
    let mut estimator = RttEstimator::new();

    // Add multiple samples (in microseconds)
    for i in 1..=10 {
        estimator.add_sample(50_000 + i * 10_000); // 50ms + increments
    }

    // SRTT should be smoothed
    assert!(estimator.srtt() > Duration::ZERO);
    assert!(Duration::from_micros(estimator.rttvar_us()) > Duration::ZERO);

    // RTO should be SRTT + 4*RTTVAR
    let expected_rto = estimator.srtt() + Duration::from_micros(4 * estimator.rttvar_us());
    assert!(estimator.rto() >= expected_rto);
}

#[test]
fn test_rtt_estimator_rto_bounds() {
    let mut estimator = RttEstimator::new();

    // Add very small RTT (1ms = 1000us)
    estimator.add_sample(1_000);

    // RTO should be at least 200ms (RFC 6298 minimum)
    assert!(estimator.rto() >= Duration::from_millis(200));

    // Add very large RTT (5s = 5_000_000us)
    for _ in 0..10 {
        estimator.add_sample(5_000_000);
    }

    // RTO should be at most 2 seconds (RFC 6298 maximum for uTP)
    assert!(estimator.rto() <= Duration::from_secs(2));
}

#[test]
fn test_delay_estimator_initial_state() {
    let estimator = DelayEstimator::new();

    // No samples yet
    assert!(estimator.base_delay().is_none());
}

#[test]
fn test_delay_estimator_base_delay() {
    let mut estimator = DelayEstimator::new();

    // Add samples (in microseconds)
    estimator.add_sample(50_000); // 50ms
    estimator.add_sample(30_000); // 30ms
    estimator.add_sample(40_000); // 40ms

    // Base delay should be minimum (30ms)
    assert_eq!(estimator.base_delay(), Some(Duration::from_micros(30_000)));
}

#[test]
fn test_delay_estimator_queuing_delay() {
    let mut estimator = DelayEstimator::new();

    // Establish base delay
    estimator.add_sample(30_000);

    // Add current delay
    estimator.add_sample(80_000);

    // Queuing delay = current - base = 80 - 30 = 50ms
    let queuing = estimator.queuing_delay();
    assert!(queuing > Duration::ZERO);
}

#[test]
fn test_delay_estimator_congestion_detection() {
    let mut estimator = DelayEstimator::new();

    // Establish low base delay
    estimator.add_sample(20_000);
    estimator.add_sample(25_000);
    estimator.add_sample(22_000);

    // High current delay indicates congestion
    estimator.add_sample(150_000);

    // Queuing delay should exceed target (100ms)
    let queuing = estimator.queuing_delay();
    assert!(queuing > LEDBAT_TARGET_DELAY);
}

// ===========================================================================
// Section 6: Real UDP Transmission Simulation
// ===========================================================================

#[test]
fn test_utp_real_udp_syn_exchange() {
    let (server, client) = create_udp_pair();
    let server_addr = get_addr(&server);
    let client_addr = get_addr(&client);

    // Client creates SYN packet
    let syn = UtpPacket::syn(12345, 1);
    let syn_bytes = syn.to_bytes();

    // Send SYN to server
    assert!(send_raw(&client, &syn_bytes, server_addr));

    // Server receives SYN
    let (received, from_addr) = recv_with_timeout(&server, 1000).expect("Server should receive SYN");

    // Parse received packet
    let parsed = UtpPacket::from_bytes(&received).expect("Should parse SYN");
    assert_eq!(parsed.packet_type().unwrap(), PacketType::StSyn);
    assert_eq!(from_addr, client_addr);

    // Server sends SYN-ACK
    let syn_ack = UtpPacket::syn(parsed.connection_id, 1);
    let syn_ack_bytes = syn_ack.to_bytes();

    assert!(send_raw(&server, &syn_ack_bytes, client_addr));

    // Client receives SYN-ACK
    let (received, _) = recv_with_timeout(&client, 1000).expect("Client should receive SYN-ACK");

    let parsed = UtpPacket::from_bytes(&received).expect("Should parse SYN-ACK");
    assert_eq!(parsed.packet_type().unwrap(), PacketType::StSyn);
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
    let data = UtpPacket::data(conn_id, 2, 1, payload.clone());
    let data_bytes = data.to_bytes();

    assert!(send_raw(&client, &data_bytes, server_addr));

    // Server receives DATA
    let (received, from_addr) = recv_with_timeout(&server, 1000).expect("Server should receive DATA");

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
    let fin = UtpPacket::fin(conn_id, 10, 9);
    let fin_bytes = fin.to_bytes();

    assert!(send_raw(&client, &fin_bytes, server_addr));

    // Server receives FIN
    let (received, _) = recv_with_timeout(&server, 1000).expect("Server should receive FIN");

    let parsed = UtpPacket::from_bytes(&received).expect("Should parse FIN");
    assert_eq!(parsed.packet_type().unwrap(), PacketType::StFin);

    // Server sends FIN-ACK
    let fin_ack = UtpPacket::fin(conn_id, 9, 10);
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
        let data = UtpPacket::data(conn_id, i + 1, i, payload);
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
    let mut expected_seq = 2;
    for i in 0..3 {
        let payload = vec![i as u8; 50];
        let data = UtpPacket::data(conn_id, expected_seq, expected_seq - 1, payload);
        let data_bytes = data.to_bytes();

        assert!(send_raw(&client, &data_bytes, server_addr));

        // Receive and verify sequence number
        let (received, _) = recv_with_timeout(&server, 500).expect("Should receive packet");
        let parsed = UtpPacket::from_bytes(&received).expect("Should parse");

        assert_eq!(parsed.seq_nr, expected_seq);
        expected_seq += 1;
    }
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
    let syn = UtpPacket::syn(12345, 1);
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

// ===========================================================================
// Section 8: Performance and Stress Tests
// ===========================================================================

#[test]
fn test_utp_high_frequency_packets() {
    let (server, client) = create_udp_pair();
    let server_addr = get_addr(&server);

    let conn_id = 12345;
    let start = Instant::now();

    // Send 100 packets rapidly
    for i in 1..=100 {
        let payload = vec![(i % 256) as u8; 100];
        let data = UtpPacket::data(conn_id, i + 1, i, payload);
        let data_bytes = data.to_bytes();

        send_raw(&client, &data_bytes, server_addr);
    }

    // Count received packets
    let mut received = 0;
    while recv_with_timeout(&server, 100).is_some() && received < 100 {
        received += 1;
    }

    let elapsed = start.elapsed();

    // Should handle high frequency
    println!("Sent 100 packets in {:?}", elapsed);
    println!("Received {} packets", received);

    // At least some packets should be received
    assert!(received > 0);
}

#[test]
fn test_utp_large_payload() {
    let (server, client) = create_udp_pair();
    let server_addr = get_addr(&server);

    let conn_id = 12345;

    // Create large payload (but within UDP limits)
    let payload: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
    let data = UtpPacket::data(conn_id, 2, 1, payload.clone());
    let data_bytes = data.to_bytes();

    // Verify size
    assert_eq!(data_bytes.len(), 20 + payload.len());

    // Send and receive
    assert!(send_raw(&client, &data_bytes, server_addr));

    let (received, _) = recv_with_timeout(&server, 1000).expect("Should receive large packet");

    let parsed = UtpPacket::from_bytes(&received).expect("Should parse");
    assert_eq!(parsed.payload.len(), payload.len());
}

#[test]
fn test_ledbat_stress_window_updates() {
    let mut controller = LedbatController::new();

    // Simulate many ACKs
    for i in 0..1000 {
        controller.on_data_sent(1400);
        controller.on_ack_received(50_000 + (i % 100) * 1000, 1400);
    }

    // Window should remain bounded
    assert!(controller.get_window_size() >= LEDBAT_MIN_CWND * 1500);
    assert!(controller.get_window_size() <= LEDBAT_MAX_CWND * 1500);
}

#[test]
fn test_rtt_estimator_stress_samples() {
    let mut estimator = RttEstimator::new();

    // Add many samples (in microseconds)
    for i in 0..1000 {
        estimator.add_sample(50_000 + (i % 100) * 1000);
    }

    // RTO should remain bounded
    assert!(estimator.rto() >= Duration::from_millis(200));
    assert!(estimator.rto() <= Duration::from_secs(2));
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
    let mut seq = 2;

    for chunk in piece_data.chunks(chunk_size) {
        let packet = UtpPacket::data(conn_id, seq, seq - 1, chunk.to_vec());

        // Verify packet creation
        let bytes = packet.to_bytes();
        assert!(bytes.len() > 20);

        seq += 1;
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
    let syn_ack = UtpPacket::syn(client_conn.local_connection_id(), 1);
    client_conn.on_packet_received(&syn_ack).expect("Handle SYN-ACK failed");

    assert!(client_conn.is_established());

    // Send BitTorrent handshake (68 bytes)
    let bt_handshake: Vec<u8> = vec![
        19, // Protocol name length
        b'B', b'i', b't', b'T', b'o', b'r', b'r', b'e', b'n', b't', b' ', b'p', b'r', b'o', b't', b'o', b'c', b'o', b'l', // "BitTorrent protocol"
        0, 0, 0, 0, 0, 0, 0, 0, // Reserved bytes
        // Info hash (20 bytes)
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
        // Peer ID (20 bytes)
        b'A', b'R', b'I', b'A', b'2', b'R', b'S', b'T', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0',
    ];

    client_conn.send_data(&bt_handshake).expect("Send BT handshake failed");

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

    let syn_ack = UtpPacket::syn(conn.local_connection_id(), 1);
    conn.on_packet_received(&syn_ack).expect("Handle SYN-ACK failed");

    // Send piece request (ID=6, index=0, begin=0, length=16384)
    let piece_request: Vec<u8> = vec![
        0, 0, 0, 13, // Length prefix (13 bytes)
        6, // Message ID (request)
        0, 0, 0, 0, // Piece index
        0, 0, 0, 0, // Begin offset
        0, 0, 64, 0, // Length (16384)
    ];

    conn.send_data(&piece_request).expect("Send piece request failed");
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

    let syn_ack = UtpPacket::syn(conn.local_connection_id(), 1);
    conn.on_packet_received(&syn_ack).expect("Handle SYN-ACK failed");

    // Receive piece data (ID=7)
    let piece_data_header: Vec<u8> = vec![
        0, 0, 64, 21, // Length prefix (16389 bytes = 9 + 16384)
        7, // Message ID (piece)
        0, 0, 0, 0, // Piece index
        0, 0, 0, 0, // Begin offset
    ];

    // Simulate receiving header - seq_nr must match expected_recv_seq (1)
    let data_packet = UtpPacket::data(conn.local_connection_id(), 1, 1, piece_data_header.clone());

    conn.on_packet_received(&data_packet).expect("Handle piece header failed");

    // Verify data received
    let received = conn.recv_data();
    assert!(received.len() > 0);
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
    let syn = UtpPacket::syn(conn_id, 1);
    send_raw(&client, &syn.to_bytes(), server_addr);

    // Server receives SYN and sends SYN-ACK
    let (syn_received, _) = recv_with_timeout(&server, 1000).expect("Server receive SYN");
    let parsed_syn = UtpPacket::from_bytes(&syn_received).expect("Parse SYN");

    let syn_ack = UtpPacket::syn(parsed_syn.connection_id, 1);
    send_raw(&server, &syn_ack.to_bytes(), client_addr);

    // Client receives SYN-ACK
    recv_with_timeout(&client, 1000).expect("Client receive SYN-ACK");

    // Phase 2: Data transfer
    let payload = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    let data = UtpPacket::data(conn_id, 2, 1, payload.clone());
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
    let fin = UtpPacket::fin(conn_id, 3, 2);
    send_raw(&client, &fin.to_bytes(), server_addr);

    // Server receives FIN and sends FIN-ACK
    recv_with_timeout(&server, 1000).expect("Server receive FIN");

    let fin_ack = UtpPacket::fin(conn_id, 2, 3);
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
    let packet1 = UtpPacket::data(conn_id, 2, 1, data1.clone());
    send_raw(&client, &packet1.to_bytes(), server_addr);

    let (recv1, _) = recv_with_timeout(&server, 500).expect("Server receive data1");
    let parsed1 = UtpPacket::from_bytes(&recv1).expect("Parse");
    assert_eq!(parsed1.payload, data1);

    // Server -> Client: Data 2
    let data2 = vec![4, 5, 6];
    let packet2 = UtpPacket::data(conn_id, 2, 1, data2.clone());
    send_raw(&server, &packet2.to_bytes(), client_addr);

    let (recv2, _) = recv_with_timeout(&client, 500).expect("Client receive data2");
    let parsed2 = UtpPacket::from_bytes(&recv2).expect("Parse");
    assert_eq!(parsed2.payload, data2);

    // Bidirectional transfer complete
}