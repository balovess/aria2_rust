use super::*;

#[test]
fn test_controller_initialization() {
    let controller = LedbatController::new();

    assert_eq!(controller.get_window_size(), LEDBAT_MIN_CWND * DEFAULT_MSS);
    assert_eq!(controller.get_bytes_in_flight(), 0);
    assert!(controller.can_send());
    assert!(controller.is_slow_start());
    assert_eq!(controller.get_target_delay(), LEDBAT_TARGET_DELAY);
}

#[test]
fn test_controller_with_custom_mss() {
    let controller = LedbatController::with_mss(1400);
    assert_eq!(controller.get_mss(), 1400);
    assert_eq!(controller.get_window_size(), LEDBAT_MIN_CWND * 1400);
}

#[test]
fn test_controller_with_custom_target_delay() {
    let controller = LedbatController::with_target_delay(Duration::from_millis(200));
    assert_eq!(controller.get_target_delay(), Duration::from_millis(200));
}

#[test]
fn test_on_data_sent() {
    let mut controller = LedbatController::new();

    controller.on_data_sent(1500);
    assert_eq!(controller.get_bytes_in_flight(), 1500);
    assert!(controller.time_since_last_send().is_some());

    controller.on_data_sent(500);
    assert_eq!(controller.get_bytes_in_flight(), 2000);
}

#[test]
fn test_can_send() {
    let mut controller = LedbatController::new();

    // Initially can send
    assert!(controller.can_send());

    // Fill the window
    let window = controller.get_window_size();
    controller.on_data_sent(window);
    assert!(!controller.can_send());

    // ACK some data
    controller.on_ack_received(50_000, 1500);
    assert!(controller.can_send());
}

#[test]
fn test_available_window() {
    let mut controller = LedbatController::new();

    let initial_window = controller.available_window();
    assert_eq!(initial_window, controller.get_window_size());

    controller.on_data_sent(1500);
    let available = controller.available_window();
    assert_eq!(available, controller.get_window_size() - 1500);
}

#[test]
fn test_slow_start_increase() {
    let mut controller = LedbatController::new();

    let initial_cwnd = controller.get_window_size();

    // In slow start, cwnd should increase by bytes_acked
    controller.on_ack_received(50_000, 1500);

    // cwnd should increase
    assert!(controller.get_window_size() > initial_cwnd);
    assert!(controller.is_slow_start());
}

#[test]
fn test_slow_start_exit_on_target_delay() {
    let mut controller = LedbatController::new();

    // Send enough to fill window
    controller.on_data_sent(controller.get_window_size());

    // First, establish a low base delay with multiple samples
    controller.on_ack_received(50_000, 1500); // 50ms - low delay
    controller.on_ack_received(45_000, 1500); // 45ms - even lower
    controller.on_ack_received(48_000, 1500); // 48ms

    // Now we have 3 samples, base_delay should be ~45ms
    assert_eq!(
        controller.get_base_delay(),
        Some(Duration::from_micros(45_000))
    );

    // ACK with high delay (above target)
    let high_delay = LEDBAT_TARGET_DELAY.as_micros() as u64 + 50_000; // 150ms
    controller.on_ack_received(high_delay, 1500);

    // Should exit slow start because queuing_delay (150-45=105ms) > target (100ms)
    assert!(!controller.is_slow_start());
}

#[test]
fn test_slow_start_exit_on_ack_count() {
    let mut controller = LedbatController::new();

    // Send and ACK multiple times with low delay
    for _ in 0..15 {
        controller.on_data_sent(1500);
        controller.on_ack_received(50_000, 1500);
    }

    // Should exit slow start after enough ACKs
    assert!(!controller.is_slow_start());
}

#[test]
fn test_congestion_avoidance_below_target() {
    let mut controller = LedbatController::new();

    // Force exit slow start
    controller.slow_start = false;

    let initial_cwnd = controller.get_window_size();

    // ACK with queuing delay below target (should increase cwnd)
    let low_delay = 50_000; // 50ms, well below 100ms target
    controller.base_delay = Some(20_000); // 20ms base
    controller.current_delay = low_delay;

    controller.on_ack_received(low_delay, 1500);

    // cwnd should increase slightly
    assert!(controller.get_window_size() >= initial_cwnd);
}

#[test]
fn test_congestion_avoidance_above_target() {
    let mut controller = LedbatController::new();

    // Force exit slow start
    controller.slow_start = false;

    // Establish a base delay first
    controller.on_ack_received(20_000, 1500); // 20ms - this will be base
    controller.on_ack_received(25_000, 1500); // 25ms
    controller.on_ack_received(22_000, 1500); // 22ms

    // Now we have 3 samples, base_delay should be 20ms
    assert_eq!(
        controller.get_base_delay(),
        Some(Duration::from_micros(20_000))
    );

    // Get cwnd after establishing base delay
    let cwnd_before_high_delay = controller.get_window_size();

    // ACK with queuing delay above target (should decrease cwnd)
    let high_delay = 150_000; // 150ms, queuing = 130ms > target (100ms)
    controller.on_ack_received(high_delay, 1500);

    // cwnd should decrease from the value before high delay
    assert!(controller.get_window_size() < cwnd_before_high_delay);

    // Continue with high delays to further reduce cwnd
    controller.on_ack_received(high_delay, 1500);
    controller.on_ack_received(high_delay, 1500);

    // After multiple high delay ACKs, cwnd should be significantly lower
    assert!(controller.get_window_size() < cwnd_before_high_delay - 500);
}

#[test]
fn test_on_timeout() {
    let mut controller = LedbatController::new();

    // Increase cwnd first
    controller.on_data_sent(1500);
    controller.on_ack_received(50_000, 1500);
    controller.on_data_sent(1500);
    controller.on_ack_received(50_000, 1500);

    let cwnd_before = controller.get_window_size();
    assert!(cwnd_before > LEDBAT_MIN_CWND * DEFAULT_MSS);

    // Timeout
    controller.on_timeout();

    // Should reset to minimum
    assert_eq!(controller.get_window_size(), LEDBAT_MIN_CWND * DEFAULT_MSS);
    assert_eq!(controller.get_bytes_in_flight(), 0);
    assert!(controller.is_slow_start());
}

#[test]
fn test_on_loss() {
    let mut controller = LedbatController::new();

    // Increase cwnd
    controller.on_data_sent(1500);
    controller.on_ack_received(50_000, 1500);
    controller.on_data_sent(1500);
    controller.on_ack_received(50_000, 1500);

    let cwnd_before = controller.get_window_size();

    // Loss
    controller.on_loss();

    // Should reduce by half
    assert!(controller.get_window_size() <= cwnd_before / 2);
    assert!(!controller.is_slow_start());
}

#[test]
fn test_reset() {
    let mut controller = LedbatController::new();

    // Modify state
    controller.on_data_sent(1500);
    controller.on_ack_received(50_000, 1500);
    controller.on_data_sent(1500);
    controller.slow_start = false;

    // Reset
    controller.reset();

    assert_eq!(controller.get_window_size(), LEDBAT_MIN_CWND * DEFAULT_MSS);
    assert_eq!(controller.get_bytes_in_flight(), 0);
    assert!(controller.is_slow_start());
    assert!(controller.get_base_delay().is_none());
    assert_eq!(controller.get_current_delay(), Duration::ZERO);
}

#[test]
fn test_delay_measurements() {
    let mut controller = LedbatController::new();

    // First ACK
    controller.on_ack_received(50_000, 1500);
    assert_eq!(
        controller.get_base_delay(),
        Some(Duration::from_micros(50_000))
    );
    assert_eq!(
        controller.get_current_delay(),
        Duration::from_micros(50_000)
    );

    // Second ACK with higher delay
    controller.on_ack_received(80_000, 1500);
    assert_eq!(
        controller.get_base_delay(),
        Some(Duration::from_micros(50_000))
    ); // Min stays
    assert_eq!(
        controller.get_current_delay(),
        Duration::from_micros(80_000)
    );

    // Third ACK with lower delay
    controller.on_ack_received(30_000, 1500);
    assert_eq!(
        controller.get_base_delay(),
        Some(Duration::from_micros(30_000))
    ); // New min
    assert_eq!(
        controller.get_current_delay(),
        Duration::from_micros(30_000)
    );
}

#[test]
fn test_queuing_delay() {
    let mut controller = LedbatController::new();

    controller.base_delay = Some(50_000); // 50ms
    controller.current_delay = 150_000; // 150ms

    let queuing = controller.get_queuing_delay();
    assert_eq!(queuing, Duration::from_micros(100_000)); // 100ms
}

#[test]
fn test_window_bounds() {
    let mut controller = LedbatController::new();

    // Try to increase cwnd beyond max
    controller.cwnd = LEDBAT_MAX_CWND * DEFAULT_MSS + 10000;
    controller.on_ack_received(50_000, 1500);

    // Should be clamped to max
    assert_eq!(controller.get_window_size(), LEDBAT_MAX_CWND * DEFAULT_MSS);
}

#[test]
fn test_minimum_window() {
    let mut controller = LedbatController::new();

    // Force cwnd below minimum
    controller.cwnd = 100; // Below minimum
    controller.on_ack_received(200_000, 1500); // High delay to reduce cwnd

    // Should be clamped to minimum
    assert!(controller.get_window_size() >= LEDBAT_MIN_CWND * DEFAULT_MSS);
}

#[test]
fn test_bytes_in_flight_tracking() {
    let mut controller = LedbatController::new();

    // Send data
    controller.on_data_sent(1500);
    assert_eq!(controller.get_bytes_in_flight(), 1500);

    controller.on_data_sent(1500);
    assert_eq!(controller.get_bytes_in_flight(), 3000);

    // ACK some data
    controller.on_ack_received(50_000, 1500);
    assert_eq!(controller.get_bytes_in_flight(), 1500);

    // ACK remaining
    controller.on_ack_received(50_000, 1500);
    assert_eq!(controller.get_bytes_in_flight(), 0);
}

#[test]
fn test_delay_sample_count() {
    let controller = LedbatController::new();
    assert_eq!(controller.delay_sample_count(), 0);
    assert!(!controller.has_delay_samples());

    let mut controller = controller;
    controller.on_ack_received(50_000, 1500);
    assert_eq!(controller.delay_sample_count(), 1);
    assert!(controller.has_delay_samples());
}

#[test]
fn test_rfc6817_scenario() {
    // Test scenario based on RFC 6817 principles
    let mut controller = LedbatController::new();

    // Initial slow start phase with low delays
    for i in 0..3 {
        controller.on_data_sent(1500);
        controller.on_ack_received(50_000 - i * 1000, 1500); // Decreasing delays
        println!(
            "Iteration {}: cwnd = {}, slow_start = {}, base_delay = {:?}",
            i,
            controller.get_window_size(),
            controller.is_slow_start(),
            controller.get_base_delay()
        );
    }

    // Should have grown cwnd in slow start
    assert!(controller.get_window_size() > LEDBAT_MIN_CWND * DEFAULT_MSS);

    // Now we have 3 samples with base_delay ~47ms
    assert!(controller.get_base_delay().is_some());

    // Simulate congestion (high delay)
    for i in 0..3 {
        controller.on_data_sent(1500);
        controller.on_ack_received(150_000, 1500); // 150ms delay, queuing = 103ms > target
        println!(
            "Congestion {}: cwnd = {}, slow_start = {}, queuing_delay = {:?}",
            i,
            controller.get_window_size(),
            controller.is_slow_start(),
            controller.get_queuing_delay()
        );
    }

    // cwnd should have decreased due to high delay
    // and should have exited slow start
    assert!(!controller.is_slow_start());
}

#[test]
fn test_gain_factor() {
    // Verify that the gain factor is applied correctly
    let mut controller = LedbatController::new();
    controller.slow_start = false;
    controller.cwnd = 10000;

    // Establish a base delay first
    controller.on_ack_received(20_000, 1500); // 20ms - this will be base
    controller.on_ack_received(25_000, 1500); // 25ms
    controller.on_ack_received(22_000, 1500); // 22ms

    // Now we have 3 samples, base_delay should be 20ms
    assert_eq!(
        controller.get_base_delay(),
        Some(Duration::from_micros(20_000))
    );

    let initial_cwnd = controller.get_window_size();

    // Queuing delay = 70ms - 20ms = 50ms, target = 100ms
    // delay_factor = (100 - 50) / 100 = 0.5
    // delta = 0.5 * 0.5 * 1500 = 375 bytes
    controller.on_ack_received(70_000, 1500);

    let new_cwnd = controller.get_window_size();
    // cwnd should increase by approximately 375 bytes
    assert!(new_cwnd > initial_cwnd);
    assert!(new_cwnd < initial_cwnd + 500); // Reasonable bound
}

#[test]
fn test_saturating_operations() {
    let mut controller = LedbatController::new();

    // Test that operations don't overflow
    controller.on_data_sent(u32::MAX / 2);
    controller.on_data_sent(u32::MAX / 2);
    // bytes_in_flight should saturate, not overflow
    // Note: bytes_in_flight is u32, so it's always <= u32::MAX by definition

    // Test ACK with more bytes than in flight
    controller.on_ack_received(50_000, u32::MAX);
    assert_eq!(controller.get_bytes_in_flight(), 0);
}

#[test]
fn test_time_since_last_send() {
    let controller = LedbatController::new();
    assert!(controller.time_since_last_send().is_none());

    let mut controller = controller;
    controller.on_data_sent(1500);
    let elapsed = controller.time_since_last_send();
    assert!(elapsed.is_some());
    assert!(elapsed.unwrap() < Duration::from_millis(100));
}
