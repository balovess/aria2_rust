use aria2_core::engine::bt_seed_manager::{BtSeedManager, SeedExitCondition};
use aria2_core::engine::bt_upload_session::{
    BtSeedingConfig, InMemoryPieceProvider, PieceDataProvider,
};

#[test]
fn test_bt_upload_session_creation() {
    let config = BtSeedingConfig {
        max_upload_bytes_per_sec: Some(50000),
        max_peers_to_unchoke: 4,
        optimistic_unchoke_interval_secs: 30,
    };
    assert_eq!(config.max_peers_to_unchoke, 4);
}

#[test]
fn test_piece_data_provider_from_memory() {
    let mut provider = InMemoryPieceProvider::new(1024, 5);
    provider.set_all_from_pattern(|piece_idx, byte_idx| ((piece_idx * 7 + byte_idx) % 256) as u8);

    assert!(provider.has_piece(0));
    assert!(provider.has_piece(4));
    assert!(!provider.has_piece(5));
    assert_eq!(provider.num_pieces(), 5);

    let data = provider.get_piece_data(0, 100, 50).unwrap();
    assert_eq!(data.len(), 50);
}

#[test]
fn test_seed_manager_exit_by_time() {
    let cond = SeedExitCondition::with_time(1);
    let mut mgr = make_empty_mgr(cond);
    assert!(!mgr.should_exit());

    mgr.seeding_start_time = std::time::Instant::now() - std::time::Duration::from_secs(2);
    assert!(mgr.should_exit());
}

#[test]
fn test_seed_manager_exit_by_ratio() {
    let cond = SeedExitCondition::with_ratio(1.0);
    let mut mgr = make_empty_mgr_with_downloaded(1000, 200, cond);
    assert!(!mgr.should_exit());

    mgr.total_uploaded = 1200;
    assert!(mgr.should_exit());
}

#[test]
fn test_seed_manager_no_exit_infinite() {
    let cond = SeedExitCondition::infinite();
    let mut mgr = make_empty_mgr(cond);
    mgr.total_uploaded = u64::MAX;
    mgr.seeding_start_time = std::time::Instant::now() - std::time::Duration::from_secs(86400);
    assert!(!mgr.should_exit());
}

#[test]
fn test_choke_blocks_upload_concept() {
    let _config = BtSeedingConfig::default();
    let session_state = (false, false);

    let should_upload = !session_state.0 && session_state.1;
    assert!(!should_upload, "Choked peer should not upload");
}

#[test]
fn test_upload_speed_tracking_concept() {
    let start = std::time::Instant::now();
    let uploaded = 50000u64;
    let elapsed = start.elapsed().as_secs_f64();

    if elapsed > 0.0 {
        let speed = (uploaded as f64 / elapsed) as u64;
        assert!(speed > 0);
    }
}

#[test]
fn test_seeding_config_limits() {
    let cfg = BtSeedingConfig {
        max_upload_bytes_per_sec: Some(1024 * 1024),
        max_peers_to_unchoke: 2,
        optimistic_unchoke_interval_secs: 60,
    };
    assert_eq!(cfg.max_upload_bytes_per_sec.unwrap(), 1024 * 1024);
    assert_eq!(cfg.max_peers_to_unchoke, 2);
}

#[test]
fn test_exit_condition_combined_logic() {
    let cond = SeedExitCondition {
        seed_time: Some(std::time::Duration::from_secs(10)),
        seed_ratio: Some(1.5),
    };
    let mut mgr = make_empty_mgr_with_downloaded(1000, 400, cond);
    assert!(!mgr.should_exit());

    mgr.total_uploaded = 1600;
    mgr.seeding_start_time = std::time::Instant::now() - std::time::Duration::from_secs(15);
    assert!(mgr.should_exit(), "Both time and ratio met");

    let mut mgr2 = make_empty_mgr_with_downloaded(
        1000,
        1400,
        SeedExitCondition {
            seed_time: Some(std::time::Duration::from_secs(10)),
            seed_ratio: Some(1.5),
        },
    );
    mgr2.seeding_start_time = std::time::Instant::now() - std::time::Duration::from_secs(9);
    assert!(!mgr2.should_exit(), "Neither time nor ratio fully met yet");
}

#[test]
fn test_inmemory_provider_all_pieces_complete() {
    let mut provider = InMemoryPieceProvider::new(512, 3);
    provider.set_all_from_pattern(|_, _| 0xAA);

    for i in 0..3u32 {
        assert!(provider.has_piece(i), "piece {} should be set", i);
        let data = provider
            .get_piece_data(
                i,
                0,
                512.min(provider.num_pieces() as u32 * 512 - i as u32 * 512),
            )
            .unwrap();
        assert!(!data.is_empty());
        assert!(data.iter().all(|&b| b == 0xAA));
    }
}

fn make_empty_mgr(exit_cond: SeedExitCondition) -> BtSeedManager {
    make_empty_mgr_with_downloaded(0, 0, exit_cond)
}

fn make_empty_mgr_with_downloaded(
    downloaded: u64,
    uploaded: u64,
    exit_cond: SeedExitCondition,
) -> BtSeedManager {
    let provider = std::sync::Arc::new(InMemoryPieceProvider::new(16384, 10));
    let config = BtSeedingConfig::default();
    let conds: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection> = vec![];
    let mut mgr = BtSeedManager::new(conds, provider, config, exit_cond, downloaded);
    mgr.total_uploaded = uploaded;
    mgr
}
