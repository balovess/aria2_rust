use std::sync::Arc;
use std::time::Duration;

use super::super::BtMessageHandler;
use crate::engine::bt_download_execute::EndgameState;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_upload_session::{BtSeedingConfig, InMemoryPieceProvider, PieceDataProvider};
use crate::engine::choking_algorithm::{ChokingAlgorithm, ChokingConfig};
use aria2_protocol::bittorrent::message::handshake::Handshake;
use aria2_protocol::bittorrent::message::serializer::serialize;
use aria2_protocol::bittorrent::message::types::BtMessage;
use aria2_protocol::bittorrent::peer::connection::PeerAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::test]
async fn active_download_worker_serves_upload_request_on_same_peer_connection() {
    let info_hash = [0x41u8; 20];
    let local_peer_id = [0x42u8; 20];
    let remote_peer_id = [0x43u8; 20];
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_piece = vec![0xB7u8; 16];
    let locally_verified_piece = vec![0xA5u8; 16];

    let remote = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request_handshake = [0u8; 68];
        stream.read_exact(&mut request_handshake).await.unwrap();
        assert_eq!(&request_handshake[28..48], info_hash.as_slice());
        stream
            .write_all(&Handshake::new(&info_hash, &remote_peer_id).to_bytes())
            .await
            .unwrap();
        stream
            .write_all(&serialize(&BtMessage::Bitfield { data: vec![0x40] }))
            .await
            .unwrap();

        let request = read_frame(&mut stream).await;
        assert_eq!(request.first().copied(), Some(6));
        assert_eq!(u32::from_be_bytes(request[1..5].try_into().unwrap()), 1);
        assert_eq!(u32::from_be_bytes(request[5..9].try_into().unwrap()), 0);
        assert_eq!(u32::from_be_bytes(request[9..13].try_into().unwrap()), 16);
        stream
            .write_all(&serialize(&BtMessage::Interested))
            .await
            .unwrap();
        assert_eq!(read_frame(&mut stream).await.as_slice(), &[1]);
        stream
            .write_all(&serialize(&BtMessage::Request {
                request: aria2_protocol::bittorrent::message::types::PieceBlockRequest {
                    index: 0,
                    begin: 0,
                    length: 16,
                },
            }))
            .await
            .unwrap();
        stream
            .write_all(&serialize(&BtMessage::Piece {
                index: 1,
                begin: 0,
                data: remote_piece.into(),
            }))
            .await
            .unwrap();

        for _ in 0..3 {
            let frame = read_frame(&mut stream).await;
            if frame.first().copied() == Some(7) {
                assert_eq!(u32::from_be_bytes(frame[1..5].try_into().unwrap()), 0);
                assert_eq!(u32::from_be_bytes(frame[5..9].try_into().unwrap()), 0);
                return frame[9..].to_vec();
            }
        }
        panic!("local peer did not upload its verified piece")
    });

    let peer_addr = PeerAddr::new("127.0.0.1", address.port());
    let mut connection = BtPeerConn::connect_plain_with_options(
        &peer_addr,
        &info_hash,
        None,
        &local_peer_id,
        Duration::from_secs(5),
        false,
    )
    .await
    .unwrap();
    connection.configure_upload_with_auto_unchoke(&BtSeedingConfig::default(), 2, 16, false);

    let mut choking_algo = ChokingAlgorithm::new(ChokingConfig::default());
    choking_algo.add_peer(connection.stats().clone());
    let mut provider = InMemoryPieceProvider::new(16, 2);
    provider.set_piece_data(0, locally_verified_piece.clone());
    let provider: Arc<dyn PieceDataProvider> = Arc::new(provider);
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        BtMessageHandler::download_piece_blocks_with_sources_and_activity_with_timeout_and_max_attempts_and_provider_and_choking(
            std::slice::from_mut(&mut connection),
            1,
            16,
            1,
            None,
            Some(Arc::clone(&provider)),
            None,
            Duration::from_secs(2),
            1,
            Some(&mut choking_algo),
        ),
    )
    .await
    .expect("active download worker timed out")
    .unwrap();

    assert_eq!(result.data, vec![0xB7; 16]);
    assert_eq!(connection.stats().uploaded_bytes, 16);
    assert!(connection.stats().upload_speed > 0.0);
    assert!(!connection.stats().am_choking);
    assert!(choking_algo.peers()[0].peer_interested);
    assert!(!choking_algo.peers()[0].am_choking);
    assert_eq!(remote.await.unwrap(), locally_verified_piece);
}

#[tokio::test]
async fn endgame_workers_duplicate_requests_and_cancel_loser_on_owned_connections() {
    let info_hash = [0x51u8; 20];
    let local_peer_id = [0x52u8; 20];
    let remote_peer_ids = [[0x53u8; 20], [0x54u8; 20]];
    let piece_data = vec![0xC7u8; 16];
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut addresses = Vec::new();
    let mut remote_tasks = Vec::new();

    for (peer_index, remote_peer_id) in remote_peer_ids.into_iter().enumerate() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        addresses.push(listener.local_addr().unwrap());
        let barrier = Arc::clone(&barrier);
        let piece_data = piece_data.clone();
        remote_tasks.push(tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request_handshake = [0u8; 68];
            stream.read_exact(&mut request_handshake).await.unwrap();
            stream
                .write_all(&Handshake::new(&info_hash, &remote_peer_id).to_bytes())
                .await
                .unwrap();
            stream
                .write_all(&serialize(&BtMessage::Bitfield { data: vec![0x80] }))
                .await
                .unwrap();

            let request = read_frame(&mut stream).await;
            assert_eq!(request.first().copied(), Some(6));
            assert_eq!(u32::from_be_bytes(request[1..5].try_into().unwrap()), 0);
            assert_eq!(u32::from_be_bytes(request[5..9].try_into().unwrap()), 0);
            assert_eq!(u32::from_be_bytes(request[9..13].try_into().unwrap()), 16);
            barrier.wait().await;

            if peer_index == 0 {
                stream
                    .write_all(&serialize(&BtMessage::Piece {
                        index: 0,
                        begin: 0,
                        data: piece_data.into(),
                    }))
                    .await
                    .unwrap();
            } else {
                let cancel = read_frame(&mut stream).await;
                assert_eq!(cancel.first().copied(), Some(8));
                assert_eq!(u32::from_be_bytes(cancel[1..5].try_into().unwrap()), 0);
                assert_eq!(u32::from_be_bytes(cancel[5..9].try_into().unwrap()), 0);
                assert_eq!(u32::from_be_bytes(cancel[9..13].try_into().unwrap()), 16);
            }
        }));
    }

    let mut connections = Vec::new();
    for address in addresses {
        let peer_addr = PeerAddr::new("127.0.0.1", address.port());
        connections.push(
            BtPeerConn::connect_plain_with_options(
                &peer_addr,
                &info_hash,
                None,
                &local_peer_id,
                Duration::from_secs(5),
                false,
            )
            .await
            .unwrap(),
        );
    }

    let mut endgame_state = EndgameState::new();
    endgame_state.enter_endgame();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        BtMessageHandler::download_piece_blocks_endgame_with_sources_and_activity_with_timeout_and_max_attempts_and_provider_and_choking(
            &mut connections,
            0,
            16,
            1,
            &mut endgame_state,
            None,
            None,
            None,
            Duration::from_secs(2),
            1,
            None,
        ),
    )
    .await
    .expect("endgame worker timed out")
    .unwrap();

    assert_eq!(result.data, vec![0xC7; 16]);
    assert!(endgame_state.get_cancel_targets(0, 0, 16).is_empty());
    for task in remote_tasks {
        task.await.unwrap();
    }
}

async fn read_frame(stream: &mut TcpStream) -> Vec<u8> {
    let mut length = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut length))
        .await
        .expect("peer frame timed out")
        .unwrap();
    let mut payload = vec![0u8; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut payload).await.unwrap();
    payload
}
