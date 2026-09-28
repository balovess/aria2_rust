mod fixtures;

use std::time::Duration;

use aria2_core::engine::bt_download_command::BtDownloadCommand;
use aria2_core::engine::command::Command;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use aria2_protocol::bittorrent::message::handshake::Handshake;
use aria2_protocol::bittorrent::message::serializer::serialize;
use aria2_protocol::bittorrent::message::types::BtMessage;
use fixtures::mock_tracker::MockTrackerServer;
use fixtures::test_torrent_builder::{build_test_torrent, expected_piece_data};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

#[tokio::test]
async fn requests_next_piece_while_current_piece_block_is_slow() {
    let directory = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_port = listener.local_addr().unwrap().port();
    let tracker = MockTrackerServer::start(peer_port).await;
    let piece_length = 16 * 1024u32;
    let piece_count = 32u32;
    let total_length = u64::from(piece_count) * u64::from(piece_length);
    let torrent = build_test_torrent(
        "cross-piece-pipeline.bin",
        total_length,
        piece_length,
        &tracker.announce_url(),
    );
    let metadata = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
        .expect("test torrent must parse");
    let info_hash = metadata.info_hash.bytes;
    let pieces = (0..piece_count)
        .map(|piece_index| expected_piece_data(piece_index, piece_length, total_length))
        .collect::<Vec<_>>();
    let expected = pieces.concat();
    let (first_request_tx, first_request_rx) = oneshot::channel();
    let (next_piece_tx, next_piece_rx) = oneshot::channel();

    let peer = tokio::spawn(async move {
        let mut stream = loop {
            let (mut candidate, _) = listener.accept().await.unwrap();
            let mut pstrlen = [0; 1];
            if candidate.read_exact(&mut pstrlen).await.is_err() || pstrlen[0] != 19 {
                continue;
            }
            let mut client_handshake = [0; 67];
            if candidate.read_exact(&mut client_handshake).await.is_err()
                || &client_handshake[..19] != b"BitTorrent protocol"
                || &client_handshake[27..47] != info_hash.as_slice()
            {
                continue;
            }
            break candidate;
        };

        let remote_peer_id = [0x5Au8; 20];
        stream
            .write_all(&Handshake::new(&info_hash, &remote_peer_id).to_bytes())
            .await
            .unwrap();
        let mut availability = serialize(&BtMessage::Bitfield {
            data: vec![0xFF; piece_count.div_ceil(8) as usize],
        });
        availability.extend(serialize(&BtMessage::Unchoke));
        stream.write_all(&availability).await.unwrap();

        let mut first_request_tx = Some(first_request_tx);
        let mut next_piece_tx = Some(next_piece_tx);
        let mut delayed_first_request = false;
        let mut delayed_piece_index = None;
        let mut delayed_request = None;
        let mut delayed_request_deadline = None;
        loop {
            let frame = if let Some(deadline) = delayed_request_deadline {
                tokio::select! {
                    frame = read_wire_frame(&mut stream) => frame,
                    _ = tokio::time::sleep_until(deadline) => {
                        if let Some((piece_index, begin, length)) = delayed_request.take() {
                            send_piece(&mut stream, piece_index, begin, length, &pieces[piece_index as usize]).await;
                        }
                        delayed_request_deadline = None;
                        continue;
                    }
                }
            } else {
                read_wire_frame(&mut stream).await
            };
            let Some(frame) = frame else {
                break;
            };
            if frame.first() != Some(&6) || frame.len() < 13 {
                continue;
            }

            let piece_index = u32::from_be_bytes(frame[1..5].try_into().unwrap());
            let begin = u32::from_be_bytes(frame[5..9].try_into().unwrap());
            let length = u32::from_be_bytes(frame[9..13].try_into().unwrap());
            if !delayed_first_request {
                delayed_first_request = true;
                delayed_piece_index = Some(piece_index);
                if let Some(tx) = first_request_tx.take() {
                    let _ = tx.send(());
                }
                delayed_request = Some((piece_index, begin, length));
                delayed_request_deadline =
                    Some(tokio::time::Instant::now() + Duration::from_millis(750));
            } else {
                let slow_piece_index = delayed_piece_index.expect("first request sets slow piece");
                if piece_index != slow_piece_index {
                    if delayed_request_deadline.is_some()
                        && let Some(tx) = next_piece_tx.take()
                    {
                        let _ = tx.send(());
                    }
                    send_piece(
                        &mut stream,
                        piece_index,
                        begin,
                        length,
                        &pieces[piece_index as usize],
                    )
                    .await;
                } else if delayed_request_deadline.is_none() {
                    send_piece(
                        &mut stream,
                        piece_index,
                        begin,
                        length,
                        &pieces[piece_index as usize],
                    )
                    .await;
                } else if piece_index == delayed_piece_index.unwrap() {
                    // Keep the first block withheld until its fixed deadline.
                }
            }
        }
    });

    let options = DownloadOptions {
        seed_time: Some(0.0),
        enable_dht: false,
        enable_public_trackers: false,
        file_allocation: Some("none".to_string()),
        ..DownloadOptions::default()
    };
    let mut command = BtDownloadCommand::new(
        GroupId::new(501),
        &torrent,
        &options,
        Some(directory.path().to_str().unwrap()),
    )
    .expect("BT command must be constructible");
    let download = tokio::spawn(async move { command.execute().await });

    tokio::time::timeout(Duration::from_secs(5), first_request_rx)
        .await
        .expect("peer did not receive the first piece request")
        .expect("first request signal was dropped");
    let requested_next_piece_early =
        tokio::time::timeout(Duration::from_millis(250), next_piece_rx)
            .await
            .is_ok();

    tokio::time::timeout(Duration::from_secs(15), download)
        .await
        .expect("BT download timed out")
        .expect("BT command task panicked")
        .expect("BT download failed");
    tokio::time::timeout(Duration::from_secs(2), peer)
        .await
        .expect("mock peer did not stop after download completion")
        .expect("mock peer task panicked");

    assert_eq!(
        std::fs::read(directory.path().join("cross-piece-pipeline.bin")).unwrap(),
        expected
    );
    assert!(
        requested_next_piece_early,
        "the next piece should be requested before the delayed block for the current piece arrives"
    );
}

async fn read_wire_frame(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut length = [0; 4];
    stream.read_exact(&mut length).await.ok()?;
    let length = u32::from_be_bytes(length) as usize;
    if length > 128 * 1024 {
        return None;
    }
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).await.ok()?;
    Some(payload)
}

async fn send_piece(
    stream: &mut TcpStream,
    piece_index: u32,
    begin: u32,
    length: u32,
    piece: &[u8],
) {
    let start = begin as usize;
    let end = start + length as usize;
    let data = piece
        .get(start..end)
        .expect("request must fit in piece")
        .to_vec();
    let message = serialize(&BtMessage::Piece {
        index: piece_index,
        begin,
        data: data.into(),
    });
    stream.write_all(&message).await.unwrap();
}
