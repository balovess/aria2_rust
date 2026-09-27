//! Swarm-owned peer actor scheduling for BitTorrent block transfers.
//!
//! Manages the process of requesting and receiving individual blocks
//! from peers during piece download.
//!
//! Piece schedulers borrow the torrent's peer swarm and never own connection
//! lifetimes.

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

pub(crate) use endgame_pipeline::download_piece_blocks_endgame;
pub(crate) use peer_actor::{PeerActorControl, PeerActorTask, PeerCommand, PeerEvent};
pub(crate) use peer_registry::PeerSwarm;
pub(crate) use pipelined::download_piece_blocks;
