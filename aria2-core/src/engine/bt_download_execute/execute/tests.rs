use super::checkpoint::checkpoint_save_due;
use super::{
    checkpoint::completed_piece_bytes, checkpoint::initial_bt_progress,
    checkpoint::legacy_progress_piece_indices, environment::parse_listen_ports,
};
use crate::engine::bt_progress_info_file::BtProgress;
use std::time::{Duration, Instant};

#[test]
fn checkpoint_save_due_honors_explicit_request_and_thresholds() {
    let start = Instant::now();
    assert!(checkpoint_save_due(true, 0, start, start));
    assert!(checkpoint_save_due(
        false,
        crate::constants::BT_CHECKPOINT_SAVE_BYTES,
        start,
        start
    ));
    assert!(checkpoint_save_due(
        false,
        0,
        start,
        start + Duration::from_secs(crate::constants::BT_CHECKPOINT_SAVE_INTERVAL_SECS)
    ));
    assert!(!checkpoint_save_due(false, 0, start, start));
}

#[test]
fn listen_port_parser_expands_original_segment_syntax() {
    assert_eq!(
        parse_listen_ports("6881-6883,6999").unwrap(),
        vec![6881, 6882, 6883, 6999]
    );
}

#[test]
fn listen_port_parser_rejects_values_outside_original_bounds() {
    assert!(parse_listen_ports("1023").is_err());
    assert!(parse_listen_ports("70000").is_err());
    assert!(parse_listen_ports("6881-").is_err());
}

#[test]
fn legacy_progress_restores_only_a_matching_layout() {
    let progress = BtProgress {
        bitfield: vec![0b1010_0000],
        piece_length: 4,
        total_size: 10,
        num_pieces: 3,
        is_torrent: true,
        ..BtProgress::default()
    };

    let indices = legacy_progress_piece_indices(&progress, 4, 10, 3).unwrap();
    assert_eq!(indices, vec![0, 2]);
    assert_eq!(completed_piece_bytes(&indices, 4, 10), 6);
    assert!(legacy_progress_piece_indices(&progress, 5, 10, 3).is_none());
    assert!(legacy_progress_piece_indices(&progress, 4, 11, 3).is_none());
}

#[test]
fn legacy_progress_rejects_set_trailing_bits() {
    let progress = BtProgress {
        bitfield: vec![0b1010_0001],
        piece_length: 4,
        total_size: 10,
        num_pieces: 3,
        is_torrent: true,
        ..BtProgress::default()
    };

    assert!(legacy_progress_piece_indices(&progress, 4, 10, 3).is_none());
}

#[test]
fn integrity_check_keeps_durable_progress_visible_while_recounting() {
    assert_eq!(initial_bt_progress(true, 1024), (0, 1024));
    assert_eq!(initial_bt_progress(false, 1024), (1024, 1024));
}
