use super::{BtStopTimeoutState, ProgressDownloadStats, progress_snapshot};
use std::time::{Duration, Instant};

#[test]
fn zero_timeout_is_disabled() {
    let start = Instant::now();
    let mut state = BtStopTimeoutState::new(start, 0);

    assert!(!state.should_halt(Some(0), 0, start + Duration::from_secs(60)));
    assert!(!state.should_halt(None, 0, start + Duration::from_secs(120)));
}

#[test]
fn completed_piece_progress_resets_timeout_checkpoint() {
    let start = Instant::now();
    let mut state = BtStopTimeoutState::new(start, 0);

    assert!(!state.should_halt(Some(2), 0, start));
    assert!(!state.should_halt(Some(2), 0, start + Duration::from_secs(1)));
    assert!(!state.should_halt(Some(2), 1, start + Duration::from_secs(1)));
    assert!(!state.should_halt(Some(2), 1, start + Duration::from_secs(2)));
    assert!(state.should_halt(Some(2), 1, start + Duration::from_secs(3)));
}

#[test]
fn progress_snapshot_preserves_completed_bitfield() {
    let snapshot = progress_snapshot(
        [0x11; 20],
        &[0b1100_0000],
        4,
        8,
        2,
        ProgressDownloadStats {
            downloaded_bytes: 8,
            uploaded_bytes: 3,
            upload_speed: 0.0,
            download_speed: 0.0,
            elapsed_seconds: 9,
        },
    );

    assert_eq!(snapshot.bitfield, vec![0b1100_0000]);
    assert_eq!(snapshot.piece_length, 4);
    assert_eq!(snapshot.total_size, 8);
    assert_eq!(snapshot.num_pieces, 2);
    assert_eq!(snapshot.upload_length, 3);
    assert_eq!(snapshot.stats.downloaded_bytes, 8);
}
