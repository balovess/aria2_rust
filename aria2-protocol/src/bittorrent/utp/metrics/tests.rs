use super::*;

#[test]
fn test_rtt_estimator_initial_state() {
    let estimator = RttEstimator::new();
    assert!(!estimator.has_samples());
    assert_eq!(estimator.srtt_us(), DEFAULT_INITIAL_RTT_MS * 1000);
}

#[test]
fn test_rtt_estimator_first_sample() {
    let mut estimator = RttEstimator::new();
    estimator.add_sample(100_000); // 100ms

    assert!(estimator.has_samples());
    assert_eq!(estimator.srtt_us(), 100_000);
    assert_eq!(estimator.min_rtt_us(), Some(100_000));
}

#[test]
fn test_rtt_estimator_multiple_samples() {
    let mut estimator = RttEstimator::new();

    // Add several samples
    estimator.add_sample(100_000); // 100ms
    estimator.add_sample(150_000); // 150ms
    estimator.add_sample(120_000); // 120ms

    assert!(estimator.has_samples());
    assert!(estimator.srtt_us() > 0);
    assert_eq!(estimator.min_rtt_us(), Some(100_000));
}

#[test]
fn rtt_variance_uses_previous_smoothed_rtt() {
    let mut estimator = RttEstimator::new();
    estimator.add_sample(100_000);
    estimator.add_sample(150_000);

    assert_eq!(estimator.srtt_us(), 106_250);
    assert_eq!(estimator.rttvar_us(), 50_000);
}

#[test]
fn test_rtt_estimator_rto() {
    let mut estimator = RttEstimator::new();
    estimator.add_sample(100_000);

    let rto = estimator.rto();
    // RTO should be at least 200ms
    assert!(rto >= Duration::from_millis(200));
    // RTO should be at most 2 seconds
    assert!(rto <= Duration::from_secs(2));
}

#[test]
fn test_rtt_estimator_reset() {
    let mut estimator = RttEstimator::new();
    estimator.add_sample(100_000);
    assert!(estimator.has_samples());

    estimator.reset();
    assert!(!estimator.has_samples());
}

#[test]
fn test_delay_estimator_initial_state() {
    let estimator = DelayEstimator::new();
    assert_eq!(estimator.current_delay_us(), 0);
    assert!(estimator.base_delay_us().is_none());
}

#[test]
fn test_delay_estimator_add_sample() {
    let mut estimator = DelayEstimator::new();

    estimator.add_sample(50_000); // 50ms
    assert_eq!(estimator.current_delay_us(), 50_000);
    assert_eq!(estimator.base_delay_us(), Some(50_000));

    estimator.add_sample(80_000); // 80ms
    assert_eq!(estimator.current_delay_us(), 80_000);
    assert_eq!(estimator.base_delay_us(), Some(50_000)); // Min stays 50ms
}

#[test]
fn test_delay_estimator_queuing_delay() {
    let mut estimator = DelayEstimator::new();

    estimator.add_sample(50_000); // Base delay
    estimator.add_sample(150_000); // Higher delay

    assert_eq!(estimator.queuing_delay_us(), 100_000); // 150 - 50 = 100ms
}

#[test]
fn test_delay_estimator_congestion() {
    let mut estimator = DelayEstimator::new();

    // Below target
    estimator.add_sample(50_000);
    assert!(!estimator.is_congested());

    // Above target (queuing delay > 100ms)
    estimator.add_sample(200_000);
    assert!(estimator.is_congested());
}

#[test]
fn test_delay_estimator_delay_offset() {
    let mut estimator = DelayEstimator::new();

    estimator.add_sample(50_000);
    estimator.add_sample(120_000);

    // Queuing delay = 120 - 50 = 70ms
    // Target = 100ms
    // Offset = 70 - 100 = -30ms
    let offset = estimator.delay_offset_us();
    assert!(offset < 0);
}

#[test]
fn test_bandwidth_estimator_initial_state() {
    let estimator = BandwidthEstimator::new();
    assert!(!estimator.has_estimate());
    assert_eq!(estimator.sample_count(), 0);
}

#[test]
fn test_bandwidth_estimator_record_bytes() {
    let mut estimator = BandwidthEstimator::new();

    // Record 1KB in 100ms window
    estimator.record_bytes(1024);

    // Should not have estimate yet (window not elapsed)
    assert!(!estimator.has_estimate());
}

#[test]
fn test_bandwidth_estimator_with_window() {
    let mut estimator = BandwidthEstimator::with_window_duration(Duration::from_millis(10));

    estimator.record_bytes(1024);

    // Wait for window to elapse
    std::thread::sleep(Duration::from_millis(15));

    estimator.update();

    assert!(estimator.has_estimate());
}

#[test]
fn test_bandwidth_estimator_bandwidth_string() {
    let mut estimator = BandwidthEstimator::with_window_duration(Duration::from_millis(10));

    // Record ~1 Mbps worth of data
    estimator.record_bytes(12_500); // ~100 Kbps in 10ms

    std::thread::sleep(Duration::from_millis(15));
    estimator.update();

    let bw_string = estimator.bandwidth_string();
    assert!(!bw_string.is_empty());
}

#[test]
fn test_bandwidth_estimator_reset() {
    let mut estimator = BandwidthEstimator::new();
    estimator.record_bytes(1024);

    estimator.reset();

    assert!(!estimator.has_estimate());
    assert_eq!(estimator.sample_count(), 0);
}

#[test]
fn test_congestion_controller_initial_state() {
    let cc = CongestionController::new();
    assert!(cc.can_send());
    assert_eq!(cc.bytes_in_flight(), 0);
}

#[test]
fn test_congestion_controller_send_and_ack() {
    let mut cc = CongestionController::new();

    cc.on_send(1500);
    assert_eq!(cc.bytes_in_flight(), 1500);

    cc.on_ack(1500, -10_000); // Negative offset = below target
    assert_eq!(cc.bytes_in_flight(), 0);
}

#[test]
fn test_congestion_controller_loss() {
    let mut cc = CongestionController::new();

    // First increase cwnd through slow start
    cc.on_ack(1500, -10_000); // ACK in slow start increases cwnd
    cc.on_ack(1500, -10_000);
    let initial_cwnd = cc.cwnd();
    assert!(initial_cwnd > 2 * 1500); // Should have grown

    cc.on_loss();

    assert!(cc.cwnd() < initial_cwnd);
    assert!(!cc.in_slow_start);
}

#[test]
fn test_congestion_controller_timeout() {
    let mut cc = CongestionController::new();

    cc.on_send(1500);
    cc.on_send(1500);
    assert_eq!(cc.bytes_in_flight(), 3000);

    cc.on_timeout();

    assert_eq!(cc.bytes_in_flight(), 0);
    assert!(cc.in_slow_start);
}

#[test]
fn test_congestion_controller_available_window() {
    let mut cc = CongestionController::new();

    let available = cc.available_window();
    assert!(available > 0);

    cc.on_send(available);
    assert_eq!(cc.available_window(), 0);
    assert!(!cc.can_send());
}
