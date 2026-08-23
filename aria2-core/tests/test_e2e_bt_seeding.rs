#![cfg(feature = "bittorrent")]

use aria2_core::engine::bt_seed_manager::{BtSeedManager, SeedExitCondition};
use aria2_core::engine::bt_upload_session::{
    BtSeedingConfig, InMemoryPieceProvider, PieceDataProvider,
};
use aria2_protocol::bittorrent::message::handshake::Handshake;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[test]
fn test_bt_upload_session_creation() {
    let config = BtSeedingConfig {
        max_upload_bytes_per_sec: Some(50000),
        global_limiter: None,
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
    mgr.seeding_start_time = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(3600))
        .unwrap_or(std::time::Instant::now());
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
        global_limiter: None,
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
            .get_piece_data(i, 0, 512.min(provider.num_pieces() * 512 - i * 512))
            .unwrap();
        assert!(!data.is_empty());
        assert!(data.iter().all(|&b| b == 0xAA));
    }
}

#[tokio::test]
async fn test_bt_download_to_seed_upload_and_ratio_exit_over_tcp() {
    let info_hash = [0x51u8; 20];
    let local_peer_id = [0x61u8; 20];
    let remote_peer_id = [0x71u8; 20];
    let piece = (0..16 * 1024)
        .map(|index| (index as u8).wrapping_mul(13))
        .collect::<Vec<_>>();
    let piece_len = piece.len();

    let mut provider = InMemoryPieceProvider::new(16 * 1024, 1);
    provider.set_piece_data(0, piece.clone());
    let provider = std::sync::Arc::new(provider);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client = tokio::spawn(async move {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(&Handshake::new(&info_hash, &remote_peer_id).to_bytes())
            .await
            .unwrap();

        let mut response = [0u8; 68];
        stream.read_exact(&mut response).await.unwrap();
        assert_eq!(Handshake::parse(&response).unwrap().info_hash, info_hash);

        let availability = read_bt_frame(&mut stream).await;
        assert_eq!(availability.first().copied(), Some(5));
        assert_eq!(availability.get(1), Some(&0x80));

        stream.write_all(&[0, 0, 0, 1, 2]).await.unwrap(); // Interested
        loop {
            let payload = read_bt_frame(&mut stream).await;
            assert!(!payload.is_empty(), "seed peer closed before unchoking");
            if payload[0] == 1 {
                break;
            }
        }

        let mut request = Vec::with_capacity(17);
        request.extend_from_slice(&13u32.to_be_bytes());
        request.push(6); // Request
        request.extend_from_slice(&0u32.to_be_bytes());
        request.extend_from_slice(&0u32.to_be_bytes());
        request.extend_from_slice(&(piece.len() as u32).to_be_bytes());
        stream.write_all(&request).await.unwrap();

        let payload = read_bt_frame(&mut stream).await;
        assert_eq!(payload.first().copied(), Some(7), "expected Piece response");
        assert_eq!(u32::from_be_bytes(payload[1..5].try_into().unwrap()), 0);
        assert_eq!(u32::from_be_bytes(payload[5..9].try_into().unwrap()), 0);
        assert_eq!(&payload[9..], piece.as_slice());
    });

    let (server_stream, _) = listener.accept().await.unwrap();
    let connection =
        aria2_protocol::bittorrent::peer::connection::PeerConnection::from_incoming_stream(
            server_stream,
            &info_hash,
            &local_peer_id,
        )
        .await
        .unwrap();

    let mut manager = BtSeedManager::new(
        vec![connection],
        provider,
        BtSeedingConfig::default(),
        SeedExitCondition::with_ratio(1.0),
        piece_len as u64,
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        manager.run_seeding_loop(),
    )
    .await
    .expect("seed manager did not finish after upload")
    .expect("seed manager returned an error");
    client.await.unwrap();

    assert_eq!(manager.total_uploaded(), piece_len as u64);
    assert!(manager.halt_requested(), "ratio exit should request halt");
    assert!(!manager.is_active(), "ratio exit should end seeding");
}

async fn read_bt_frame(stream: &mut TcpStream) -> Vec<u8> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).await.unwrap();
    let mut payload = vec![0u8; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut payload).await.unwrap();
    payload
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
