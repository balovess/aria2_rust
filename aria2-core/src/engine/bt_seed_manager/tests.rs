use super::*;
use aria2_protocol::bittorrent::peer::connection::PeerConnection;
use aria2_protocol::bittorrent::peer::incoming::IncomingConnection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

#[tokio::test]
async fn seeding_accepts_a_peer_after_download_has_no_initial_peers() {
    let provider = Arc::new(crate::engine::bt_upload_session::InMemoryPieceProvider::new(1024, 1));
    let (sender, receiver) = mpsc::channel(1);
    let mut manager = BtSeedManager::new_with_transports(
        [7u8; 20],
        Vec::new(),
        provider,
        BtSeedingConfig::default(),
        SeedExitCondition::infinite(),
        1024,
        None,
        None,
        [1u8; 20],
        Some(receiver),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client_task = tokio::spawn(async move { TcpStream::connect(address).await.unwrap() });
    let (server_stream, endpoint) = listener.accept().await.unwrap();
    let _client_stream = client_task.await.unwrap();
    let peer_connection = PeerConnection::from_stream_with_peer(server_stream, [2u8; 20]);
    sender
        .send(crate::engine::bt_peer_listener::IncomingPeer {
            connection: IncomingConnection::Plain(Box::new(peer_connection)),
            endpoint,
        })
        .await
        .unwrap();

    manager.drain_incoming_peers().await;

    assert_eq!(manager.num_sessions(), 1);
}

#[tokio::test]
async fn incoming_seed_peer_receives_piece_availability_before_interested() {
    let info_hash = [0x52u8; 20];
    let local_peer_id = [0x62u8; 20];
    let remote_peer_id = [0x72u8; 20];
    let piece = (0..16 * 1024)
        .map(|index| (index as u8).wrapping_mul(17))
        .collect::<Vec<_>>();
    let mut provider = crate::engine::bt_upload_session::InMemoryPieceProvider::new(16 * 1024, 1);
    provider.set_piece_data(0, piece.clone());
    let provider = Arc::new(provider);
    let piece_len = piece.len();
    let (sender, receiver) = mpsc::channel(1);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client = tokio::spawn(async move {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(
                &aria2_protocol::bittorrent::message::handshake::Handshake::new(
                    &info_hash,
                    &remote_peer_id,
                )
                .to_bytes(),
            )
            .await
            .unwrap();

        let mut response = [0u8; 68];
        tokio::io::AsyncReadExt::read_exact(&mut stream, &mut response)
            .await
            .unwrap();
        assert_eq!(
            aria2_protocol::bittorrent::message::handshake::Handshake::parse(&response)
                .unwrap()
                .info_hash,
            info_hash
        );

        let availability = read_bt_frame(&mut stream).await;
        assert!(matches!(availability.first(), Some(5)));
        assert_eq!(availability.get(1), Some(&0x80));

        stream.write_all(&[0, 0, 0, 1, 2]).await.unwrap();
        loop {
            let payload = read_bt_frame(&mut stream).await;
            assert!(!payload.is_empty(), "seed peer closed before unchoking");
            if payload[0] == 1 {
                break;
            }
        }

        let mut request = Vec::with_capacity(17);
        request.extend_from_slice(&13u32.to_be_bytes());
        request.push(6);
        request.extend_from_slice(&0u32.to_be_bytes());
        request.extend_from_slice(&0u32.to_be_bytes());
        request.extend_from_slice(&(piece_len as u32).to_be_bytes());
        stream.write_all(&request).await.unwrap();

        let payload = read_bt_frame(&mut stream).await;
        assert_eq!(payload.first().copied(), Some(7));
        assert_eq!(u32::from_be_bytes(payload[1..5].try_into().unwrap()), 0);
        assert_eq!(u32::from_be_bytes(payload[5..9].try_into().unwrap()), 0);
        assert_eq!(&payload[9..], piece.as_slice());
    });

    let (server_stream, endpoint) = listener.accept().await.unwrap();
    let connection =
        PeerConnection::from_incoming_stream(server_stream, &info_hash, &local_peer_id)
            .await
            .unwrap();
    sender
        .send(crate::engine::bt_peer_listener::IncomingPeer {
            connection: IncomingConnection::Plain(Box::new(connection)),
            endpoint,
        })
        .await
        .unwrap();
    drop(sender);

    let mut manager = BtSeedManager::new_with_transports(
        info_hash,
        Vec::new(),
        provider,
        BtSeedingConfig::default(),
        SeedExitCondition::with_ratio(1.0),
        16 * 1024,
        None,
        None,
        local_peer_id,
        Some(receiver),
    );
    tokio::time::timeout(Duration::from_secs(5), manager.run_seeding_loop())
        .await
        .expect("seed manager did not finish after incoming upload")
        .expect("seed manager returned an error");
    client.await.unwrap();

    assert_eq!(manager.total_uploaded(), piece_len as u64);
    assert!(manager.halt_requested(), "ratio exit should request halt");
}

async fn read_bt_frame(stream: &mut TcpStream) -> Vec<u8> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).await.unwrap();
    let mut payload = vec![0u8; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut payload).await.unwrap();
    payload
}

#[tokio::test]
async fn seeding_does_not_end_just_because_all_peers_disconnect() {
    let provider = Arc::new(crate::engine::bt_upload_session::InMemoryPieceProvider::new(1024, 1));
    let mut manager = BtSeedManager::new(
        Vec::new(),
        provider,
        BtSeedingConfig::default(),
        SeedExitCondition::with_time(1),
        1024,
    );
    let cancel = manager.cancellation_token();
    let task = tokio::spawn(async move {
        let result = manager.run_seeding_loop().await;
        (result, manager.seeding_duration(), manager.halt_requested())
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();
    let (_, duration, halt_requested) = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();

    assert!(duration >= Duration::from_millis(40));
    assert!(!halt_requested);
}
