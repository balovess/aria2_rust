//! Swarm-owned peer actor scheduling for BitTorrent block transfers.
//!
//! Manages the process of requesting and receiving individual blocks
//! from peers during piece download.
//!
//! Piece schedulers borrow the torrent's peer swarm and never own connection
//! lifetimes.

use std::time::Instant;

pub(super) async fn wait_for_deadline(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    } else {
        std::future::pending::<()>().await;
    }
}

mod download_speed;
mod endgame_pipeline;
mod endgame_requests;
mod normal;
mod normal_pipeline;
mod peer_actor;
mod peer_registry;
mod peer_request;
mod peer_snapshot;
mod pipelined;

#[cfg(test)]
mod seed_pair_tests;

pub(crate) use endgame_pipeline::download_piece_blocks_endgame;
pub(crate) use peer_actor::{
    PeerActorControl, PeerActorPayloadConfig, PeerActorTask, PeerCommand, PeerEvent,
};
pub(crate) use peer_registry::{PeerSwarm, PeerSwarmEventLease};
pub(crate) use pipelined::{download_piece_blocks, download_piece_blocks_batch};
