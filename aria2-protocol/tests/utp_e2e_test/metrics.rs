use super::*;

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
        let data = UtpPacket::data(conn_id, i + 1, i, 0, payload);
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
    let data = UtpPacket::data(conn_id, 2, 1, 0, payload.clone());
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
