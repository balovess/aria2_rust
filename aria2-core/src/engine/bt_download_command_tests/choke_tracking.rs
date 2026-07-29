use super::*;
use crate::engine::choking_algorithm::{ChokingAlgorithm, ChokingConfig};
use crate::engine::peer_stats::PeerStats;
use crate::request::request_group::DownloadOptions;
use std::net::SocketAddr;

#[test]
fn test_bt_seed_manager_integration_choking_algo_none_by_default() {
    let cmd = create_test_command();
    assert!(
        cmd.choking_algo.is_none(),
        "choking_algo should be None by default"
    );
}

#[test]
fn test_download_side_choke_tracking() {
    let mut cmd = create_test_command();

    let config = ChokingConfig {
        max_upload_slots: 4,
        ..Default::default()
    };
    let mut algo = ChokingAlgorithm::new(config);

    let addr: SocketAddr = "192.168.1.10:6881".parse().unwrap();
    let peer = PeerStats::new([0xAA; 20], addr);
    algo.add_peer(peer);
    cmd.choking_algo = Some(algo);

    assert!(
        cmd.choking_algo
            .as_ref()
            .unwrap()
            .get_peer(0)
            .unwrap()
            .peer_choking
    );

    cmd.on_peer_unchoke(0);
    assert!(
        !cmd.choking_algo
            .as_ref()
            .unwrap()
            .get_peer(0)
            .unwrap()
            .peer_choking,
        "peer_choking should be false after on_peer_unchoke"
    );

    cmd.on_peer_choke(0);
    assert!(
        cmd.choking_algo
            .as_ref()
            .unwrap()
            .get_peer(0)
            .unwrap()
            .peer_choking,
        "peer_choking should be true after on_peer_choke"
    );
}

#[test]
fn test_download_side_select_best_peer_prefers_unchoked() {
    let mut cmd = create_test_command();

    let config = ChokingConfig::default();
    let mut algo = ChokingAlgorithm::new(config);

    let addr0: SocketAddr = "10.0.0.1:6881".parse().unwrap();
    let mut p0 = PeerStats::new([0x01; 20], addr0);
    p0.peer_choking = false;
    p0.download_speed = 100000.0;

    let addr1: SocketAddr = "10.0.0.2:6881".parse().unwrap();
    let mut p1 = PeerStats::new([0x02; 20], addr1);
    p1.peer_choking = true;
    p1.download_speed = 500000.0;

    let addr2: SocketAddr = "10.0.0.3:6881".parse().unwrap();
    let mut p2 = PeerStats::new([0x03; 20], addr2);
    p2.peer_choking = false;
    p2.is_snubbed = true;
    p2.download_speed = 80000.0;

    algo.add_peer(p0);
    algo.add_peer(p1);
    algo.add_peer(p2);
    cmd.choking_algo = Some(algo);

    let best = cmd.select_best_peer_for_request();
    assert_eq!(
        best,
        Some(0),
        "Should prefer unchoked+not-snubbed peer (peer 0)"
    );
}

#[test]
fn test_snubbed_peer_handling() {
    let mut cmd = create_test_command();

    let config = ChokingConfig {
        snubbed_timeout_secs: 1,
        ..Default::default()
    };
    let mut algo = ChokingAlgorithm::new(config);

    let addr: SocketAddr = "172.16.0.5:6881".parse().unwrap();
    let peer = PeerStats::new([0xBB; 20], addr);
    algo.add_peer(peer);
    cmd.choking_algo = Some(algo);

    let snubbed = cmd.check_snubbed_peers();
    assert!(snubbed.is_empty(), "No peers should be snubbed initially");

    std::thread::sleep(std::time::Duration::from_millis(1100));
    let snubbed = cmd.check_snubbed_peers();
    assert_eq!(snubbed.len(), 1, "Peer should be snubbed after timeout");
    assert_eq!(snubbed[0], 0);

    cmd.on_data_received_from_peer(0, 1024);
    assert!(
        !cmd.choking_algo
            .as_ref()
            .unwrap()
            .get_peer(0)
            .unwrap()
            .is_snubbed,
        "Receiving data should reset snubbed status"
    );
}

#[test]
fn test_add_peer_to_tracking() {
    #[allow(unused_assignments)]
    let mut cmd = create_test_command();

    let options = DownloadOptions {
        bt_max_upload_slots: Some(4),
        ..Default::default()
    };
    let gid = GroupId::new(2);
    let torrent_bytes = build_test_torrent();
    cmd = BtDownloadCommand::new(gid, &torrent_bytes, &options, None)
        .expect("Failed to create command with choking config");

    assert!(cmd.choking_algo.is_some());

    let addr1: SocketAddr = "192.168.1.20:6881".parse().unwrap();
    let idx1 = cmd.add_peer_to_tracking([0x11; 8], addr1);
    assert_eq!(cmd.choking_algo.as_ref().unwrap().len(), 1);

    let addr2: SocketAddr = "192.168.1.21:6881".parse().unwrap();
    let idx2 = cmd.add_peer_to_tracking([0x22; 8], addr2);
    assert_eq!(cmd.choking_algo.as_ref().unwrap().len(), 2);
    assert_ne!(idx1, idx2, "Different peers should get different indices");
}

#[test]
fn test_download_command_no_choking_config() {
    let mut cmd = create_test_command();
    assert!(cmd.choking_algo.is_none());

    cmd.on_peer_choke(0);
    cmd.on_peer_unchoke(0);
    cmd.on_data_received_from_peer(0, 1024);
    let best = cmd.select_best_peer_for_request();
    assert_eq!(
        best, None,
        "Should return None when no algorithm configured"
    );
    let snubbed = cmd.check_snubbed_peers();
    assert!(snubbed.is_empty());
}

#[test]
fn test_bt_command_multiple_peer_management() {
    let mut cmd = create_test_command();

    let config = ChokingConfig {
        max_upload_slots: 2,
        ..Default::default()
    };
    let mut algo = ChokingAlgorithm::new(config);

    for i in 0..5u8 {
        let addr: SocketAddr = format!("10.0.0.{}:6881", i).parse().unwrap();
        let peer = PeerStats::new([i; 20], addr);
        algo.add_peer(peer);
    }
    cmd.choking_algo = Some(algo);

    assert_eq!(cmd.choking_algo.as_ref().unwrap().len(), 5);

    cmd.on_peer_unchoke(0);
    cmd.on_peer_unchoke(2);
    cmd.on_peer_unchoke(4);

    let unchoked_count = cmd
        .choking_algo
        .as_ref()
        .unwrap()
        .peers()
        .iter()
        .filter(|p| !p.peer_choking)
        .count();
    assert_eq!(unchoked_count, 3, "Should have 3 unchoked peers");

    for i in 0..5 {
        cmd.on_data_received_from_peer(i, 1024 * (i as u64 + 1));
    }

    let all_active = cmd
        .choking_algo
        .as_ref()
        .unwrap()
        .peers()
        .iter()
        .all(|p| !p.is_snubbed);
    assert!(
        all_active,
        "All peers should be active after receiving data"
    );
}

#[test]
fn test_bt_command_state_transitions() {
    let mut cmd = create_test_command();

    let config = ChokingConfig::default();
    let algo = ChokingAlgorithm::new(config);
    cmd.choking_algo = Some(algo);

    let addr: SocketAddr = "10.0.0.100:6881".parse().unwrap();
    let idx = cmd.add_peer_to_tracking([0xFF; 8], addr);
    assert_eq!(idx, 0, "First peer should get index 0");

    cmd.on_peer_choke(0);
    cmd.on_peer_unchoke(0);

    assert!(
        cmd.choking_algo.is_some(),
        "Command should still have choking algo after peer ops"
    );
}

#[test]
fn test_bt_command_empty_peer_selection() {
    let mut cmd = create_test_command();

    let config = ChokingConfig::default();
    let algo = ChokingAlgorithm::new(config);
    cmd.choking_algo = Some(algo);

    let best = cmd.select_best_peer_for_request();
    assert_eq!(best, None, "Empty peer list should return None");

    let addr: SocketAddr = "10.0.0.200:6881".parse().unwrap();
    let mut peer = PeerStats::new([0xCC; 20], addr);
    peer.peer_choking = true;

    if let Some(ref mut algo) = cmd.choking_algo {
        algo.add_peer(peer);
    }

    let best_after_add = cmd.select_best_peer_for_request();
    assert!(
        best_after_add.is_some(),
        "Should return a peer even if all are choked"
    );
}

#[test]
fn test_bt_command_rapid_peer_state_changes() {
    let mut cmd = create_test_command();

    let config = ChokingConfig::default();
    let mut algo = ChokingAlgorithm::new(config);

    let addr: SocketAddr = "10.0.0.50:6881".parse().unwrap();
    let peer = PeerStats::new([0xDD; 20], addr);
    algo.add_peer(peer);
    cmd.choking_algo = Some(algo);

    for _ in 0..100 {
        cmd.on_peer_unchoke(0);
        cmd.on_peer_choke(0);
    }

    let final_state = cmd.choking_algo.as_ref().unwrap().get_peer(0).unwrap();
    assert!(
        final_state.peer_choking,
        "After rapid choke/unchoke cycles, peer should end up choked"
    );
}
