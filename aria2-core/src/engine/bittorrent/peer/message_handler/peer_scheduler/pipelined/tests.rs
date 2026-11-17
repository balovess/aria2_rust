use std::sync::Arc;
use std::time::Duration;

use super::super::{download_piece_blocks, download_piece_blocks_endgame};
use crate::engine::bittorrent::download::execute::EndgameState;
use crate::engine::bittorrent::peer::choking_algorithm::{ChokingAlgorithm, ChokingConfig};
use crate::engine::bittorrent::peer::connection::BtPeerConn;
use crate::engine::bittorrent::peer::upload_session::{
    BtSeedingConfig, InMemoryPieceProvider, PieceDataProvider,
};
use aria2_protocol::bittorrent::message::handshake::Handshake;
use aria2_protocol::bittorrent::message::serializer::serialize;
use aria2_protocol::bittorrent::message::types::BtMessage;
use aria2_protocol::bittorrent::peer::connection::PeerAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[path = "tests/choke_endgame.rs"]
mod choke_endgame;
#[path = "tests/retry_discovery.rs"]
mod retry_discovery;
#[path = "tests/sessions.rs"]
mod sessions;

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

async fn wait_for_request_and_unchoke(stream: &mut TcpStream) {
    let mut unchoked = false;
    loop {
        let frame = read_frame(stream).await;
        match frame.first().copied() {
            Some(1) => unchoked = true,
            Some(6) => break,
            _ => {}
        }
    }
    if !unchoked {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if read_frame(stream).await.first().copied() == Some(1) {
                    return;
                }
            }
        })
        .await
        .expect("choke scheduler did not unchoke the interested peer");
    }
}
