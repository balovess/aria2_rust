//! Tests for keep-alive timer, flooding detection, message received processing,
//! have index tracking, and checkHave optimization.

use super::super::BtPeerInteractive;
use super::super::types::*;
use super::{instant_past, make_test_conn};

// ── Keep-alive timer tests ─────────────────────────────────────────

#[test]
fn test_keep_alive_timer_initially_not_needed() {
    let info_hash = [0u8; 20];
    let interactive = BtPeerInteractive::new(info_hash, 100);
    // Just created — should not need keepalive yet
    assert!(!interactive.should_send_keepalive());
}

#[test]
fn test_keep_alive_timer_after_interval() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    // Force timer to past
    interactive.keep_alive_timer = instant_past(130);
    assert!(interactive.should_send_keepalive());
}

#[test]
fn test_keep_alive_timer_reset() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.keep_alive_timer = instant_past(130);
    assert!(interactive.should_send_keepalive());
    interactive.reset_keep_alive_timer();
    assert!(!interactive.should_send_keepalive());
}

#[test]
fn test_keep_alive_custom_interval() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.set_keep_alive_interval(60);
    interactive.keep_alive_timer = instant_past(65);
    assert!(interactive.should_send_keepalive());
}

// ── Flooding detection tests ───────────────────────────────────────

#[test]
fn test_flooding_detection_no_flooding() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    // Single choke/unchoke — not flooding
    interactive.on_message_received(0, false); // Choke, was not choking
    // Interval not elapsed yet
    assert!(!interactive.detect_flooding());
}

#[test]
fn test_flooding_detection_choke_flooding() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    // Two choke/unchoke transitions → flooding
    interactive.on_message_received(0, false); // Choke, was not choking
    interactive.on_message_received(1, true); // Unchoke, was choking
    // Force both outer timer and inner FloodingStat timer elapsed
    interactive.flooding_timer = instant_past(6);
    interactive.flooding_stat.last_reset = instant_past(6);
    assert!(interactive.detect_flooding());
}

#[test]
fn test_flooding_detection_keepalive_flooding() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.on_keepalive_received();
    interactive.on_keepalive_received();
    // Force both outer timer and inner FloodingStat timer elapsed
    interactive.flooding_timer = instant_past(6);
    interactive.flooding_stat.last_reset = instant_past(6);
    assert!(interactive.detect_flooding());
}

#[test]
fn test_flooding_detection_reset_after_interval() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.on_message_received(0, false);
    interactive.on_message_received(1, true);
    interactive.flooding_timer = instant_past(6);
    interactive.flooding_stat.last_reset = instant_past(6);
    assert!(interactive.detect_flooding());
    // After detection, stats are reset — no more flooding
    assert!(!interactive.detect_flooding());
}

// ── Message received processing tests ──────────────────────────────

#[test]
fn test_on_message_received_choke() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    // Peer was not choking, then sends Choke → transition detected
    interactive.on_message_received(0, false);
    assert_eq!(interactive.flooding_stat.choke_unchoke_count(), 1);

    // Peer was already choking, sends Choke again → no transition
    interactive.on_message_received(0, true);
    assert_eq!(interactive.flooding_stat.choke_unchoke_count(), 1);
}

#[test]
fn test_on_message_received_unchoke() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    // Peer was choking, sends Unchoke → transition detected
    interactive.on_message_received(1, true);
    assert_eq!(interactive.flooding_stat.choke_unchoke_count(), 1);

    // Peer was not choking, sends Unchoke → no transition
    interactive.on_message_received(1, false);
    assert_eq!(interactive.flooding_stat.choke_unchoke_count(), 1);
}

#[test]
fn test_on_message_received_data_exchange() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    // Request (ID=6) and Piece (ID=7) record data exchange
    interactive.on_message_received(6, false);
    interactive.on_message_received(7, false);
    // The active_interaction_checker's last_data_exchange was reset
    // We can verify by checking that a subsequent check doesn't
    // immediately return NoDataExchange
}

#[test]
fn test_on_keepalive_received() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.on_keepalive_received();
    interactive.on_keepalive_received();
    assert_eq!(interactive.flooding_stat.keepalive_count(), 2);
}

// ── Max outstanding request scaling tests ──────────────────────────

#[test]
fn test_scale_max_outstanding_request() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    assert_eq!(interactive.max_outstanding_request(), 6);

    // Lost >= 1/4 of outstanding requests → scale up
    // old=6, new=3, diff=3, diff*4=12 >= 6
    interactive.scale_max_outstanding_request(6, 3, false);
    assert_eq!(interactive.max_outstanding_request(), 12);
}

#[test]
fn test_scale_max_outstanding_request_capped() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.max_outstanding_request = 200;

    // Would go to 400, capped at UB=256
    interactive.scale_max_outstanding_request(200, 100, false);
    assert_eq!(
        interactive.max_outstanding_request(),
        UB_MAX_OUTSTANDING_REQUEST
    );
}

#[test]
fn test_scale_max_outstanding_request_no_scale_in_endgame() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    // In end-game, don't scale
    interactive.scale_max_outstanding_request(6, 0, true);
    assert_eq!(interactive.max_outstanding_request(), 6);
}

#[test]
fn test_scale_max_outstanding_request_no_scale_small_loss() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    // Lost only 1 request: diff=1, diff*4=4 < 6 → no scale
    interactive.scale_max_outstanding_request(6, 5, false);
    assert_eq!(interactive.max_outstanding_request(), 6);
}

// ── Have index tracking tests ──────────────────────────────────────

#[test]
fn test_have_index_tracking() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    assert_eq!(interactive.last_have_index(), 0);

    interactive.set_last_have_index(42);
    assert_eq!(interactive.last_have_index(), 42);

    interactive.set_last_have_index(100);
    assert_eq!(interactive.last_have_index(), 100);
}

// ── check_have returns empty without piece storage ─────────────────

#[test]
fn test_check_have_returns_empty() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    let have = interactive.check_have();
    assert!(have.is_empty());
}

// ── checkHave optimization tests ──────────────────────────────────────

#[test]
fn test_check_have_result_none_when_no_indexes() {
    let mut interactive = BtPeerInteractive::new([0u8; 20], 100);
    let result = interactive.check_have_optimized(
        &|_last| (Vec::new(), 0u64), // no new pieces
        100,                         // bitfield_length
        false,                       // fast_ext
        false,                       // all_done
        0,                           // completed_len
    );
    assert_eq!(result, CheckHaveResult::None);
}

#[test]
fn test_check_have_result_bitfield_when_many_indexes() {
    let mut interactive = BtPeerInteractive::new([0u8; 20], 100);
    // 20 Have messages = 20 * 9 = 180 bytes
    // Bitfield = 5 + 10 = 15 bytes
    // Condition: 5 + 10 <= 20 * 9 → true → use Bitfield
    let indexes: Vec<usize> = (0..20).collect();
    let result = interactive.check_have_optimized(
        &|_last| (indexes.clone(), 20u64),
        10,    // bitfield_length=10 → 5+10=15 <= 180
        false, // fast_ext
        false, // all_done
        1024,  // completed_len > 0
    );
    assert_eq!(result, CheckHaveResult::Bitfield);
}

#[test]
fn test_check_have_result_have_all_when_fast_ext_and_complete() {
    let mut interactive = BtPeerInteractive::new([0u8; 20], 100);
    let indexes: Vec<usize> = (0..20).collect();
    let result = interactive.check_have_optimized(
        &|_last| (indexes.clone(), 20u64),
        10,
        true, // fast_ext enabled
        true, // all done
        1024,
    );
    assert_eq!(result, CheckHaveResult::HaveAll);
}

#[test]
fn test_check_have_result_have_indexes_when_few() {
    let mut interactive = BtPeerInteractive::new([0u8; 20], 100);
    // 2 Have messages = 2 * 9 = 18 bytes
    // Bitfield = 5 + 100 = 105 bytes
    // Condition: 5 + 100 <= 2 * 9 → false → use Have messages
    let indexes = vec![0usize, 1];
    let result = interactive.check_have_optimized(
        &|_last| (indexes.clone(), 2u64),
        100, // bitfield_length=100 → 5+100=105 > 18
        false,
        false,
        1024,
    );
    assert_eq!(result, CheckHaveResult::HaveIndexes(indexes));
}
