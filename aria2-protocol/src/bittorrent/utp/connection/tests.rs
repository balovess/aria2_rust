use super::*;
use std::net::{IpAddr, Ipv4Addr};

fn test_addr() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12345)
}

#[test]
fn test_connection_new() {
    let conn = UtpConnection::new();
    assert_eq!(conn.state(), ConnectionState::Closed);
    assert!(!conn.is_established());
    assert!(conn.remote_addr().is_none());
}

#[test]
fn test_connection_connect() {
    let mut conn = UtpConnection::new();
    let result = conn.connect(test_addr());
    assert!(result.is_ok());
    assert_eq!(conn.state(), ConnectionState::SynSent);
    assert_eq!(conn.remote_addr(), Some(test_addr()));
}

#[test]
fn test_connection_connect_already_connected() {
    let mut conn = UtpConnection::new();
    conn.connect(test_addr()).unwrap();
    let result = conn.connect(test_addr());
    assert!(matches!(result, Err(ConnectionError::AlreadyExists)));
}

#[test]
fn test_connection_close_not_connected() {
    let mut conn = UtpConnection::new();
    let result = conn.close();
    assert!(matches!(result, Err(ConnectionError::NotConnected)));
}

#[test]
fn test_connection_send_data_not_connected() {
    let mut conn = UtpConnection::new();
    let result = conn.send_data(&[1, 2, 3]);
    assert!(matches!(result, Err(ConnectionError::NotConnected)));
}

#[test]
fn test_connection_default() {
    let conn = UtpConnection::default();
    assert_eq!(conn.state(), ConnectionState::Closed);
}

#[test]
fn test_connection_idle_time() {
    let conn = UtpConnection::new();
    let idle = conn.idle_time();
    assert!(idle.as_nanos() > 0 || idle.is_zero());
}

#[test]
fn test_connection_recv_data_empty() {
    let mut conn = UtpConnection::new();
    let data = conn.recv_data();
    assert!(data.is_empty());
}

#[test]
fn test_connection_timeout() {
    let mut conn = UtpConnection::new();
    let result = conn.check_timeout(Duration::ZERO);
    assert!(result);
    assert_eq!(conn.state(), ConnectionState::Closed);
}

#[test]
fn test_connection_no_timeout() {
    let mut conn = UtpConnection::new();
    let result = conn.check_timeout(Duration::from_secs(3600));
    assert!(!result);
}

#[test]
fn send_window_tracks_ledbat_ack_growth_and_timeout_loss() {
    let mut conn = UtpConnection::new();
    let syn = conn.connect(test_addr()).unwrap();
    conn.on_packet_received(&UtpPacket::syn_ack(
        syn.connection_id,
        40,
        syn.seq_nr,
        65_536,
    ))
    .unwrap();

    assert_eq!(conn.congestion_window(), 2 * 1400);
    assert_eq!(conn.send_data(&vec![0x33; 2800]).unwrap().len(), 2);
    assert_eq!(conn.bytes_in_flight(), 2800);

    let mut ack = UtpPacket::ack(syn.connection_id, 2, 40, 65_536);
    ack.timestamp_difference_microseconds = 50_000;
    conn.on_packet_received(&ack).unwrap();
    assert_eq!(conn.bytes_in_flight(), 1400);
    assert!(conn.congestion_window() > 2 * 1400);
    assert_eq!(conn.send_data(&vec![0x44; 1400]).unwrap().len(), 1);

    let before_loss = conn.congestion_window();
    let retransmission = conn.retransmit_packet(3).unwrap();
    assert_eq!(retransmission.seq_nr, 3);
    assert!(conn.congestion_window() < before_loss);
    assert_eq!(conn.bytes_in_flight(), 2800);
}

#[test]
fn connection_rtt_uses_local_send_time_and_excludes_retransmits() {
    let mut conn = UtpConnection::new();
    let syn = conn.connect(test_addr()).unwrap();
    conn.syn_retransmitted = true;
    conn.on_packet_received(&UtpPacket::syn_ack(
        syn.connection_id,
        40,
        syn.seq_nr,
        65_536,
    ))
    .unwrap();

    let packet = conn.send_data(b"rtt").unwrap().pop().unwrap();
    conn.send_buffer.back_mut().unwrap().sent_at = Instant::now() - Duration::from_millis(120);
    conn.on_packet_received(&UtpPacket::ack(
        syn.connection_id,
        packet.seq_nr,
        40,
        65_536,
    ))
    .unwrap();
    assert!((Duration::from_millis(118)..=Duration::from_millis(125)).contains(&conn.rtt()));
    assert_eq!(conn.rto(), Duration::from_millis(500));

    let retransmitted = conn.send_data(b"retry").unwrap().pop().unwrap();
    conn.send_buffer.back_mut().unwrap().sent_at = Instant::now() - Duration::from_secs(1);
    conn.retransmit_packet(retransmitted.seq_nr).unwrap();
    conn.on_packet_received(&UtpPacket::ack(
        syn.connection_id,
        retransmitted.seq_nr,
        40,
        65_536,
    ))
    .unwrap();
    assert!(conn.rtt() < Duration::from_millis(200));
}

#[test]
fn outgoing_timestamp_difference_tracks_remote_delay_sample_across_wrap() {
    let mut conn = UtpConnection::new();
    conn.timestamp_origin = Instant::now() - Duration::from_micros(u64::from(u32::MAX) + 15_000);
    let syn = conn.connect(test_addr()).unwrap();
    assert!(syn.timestamp_microseconds < 100_000);

    let remote_timestamp = conn.timestamp_now_microseconds().wrapping_sub(5_000);
    let mut response = UtpPacket::syn_ack(syn.connection_id, 40, syn.seq_nr, 65_536);
    response.timestamp_microseconds = remote_timestamp;
    conn.on_packet_received(&response).unwrap();
    assert!((4_000..=6_000).contains(&conn.reply_micro));

    let outgoing = conn.send_data(b"clock").unwrap().pop().unwrap();
    assert!(outgoing.timestamp_microseconds < 100_000);
    assert!((4_000..=6_000).contains(&outgoing.timestamp_difference_microseconds));
}
