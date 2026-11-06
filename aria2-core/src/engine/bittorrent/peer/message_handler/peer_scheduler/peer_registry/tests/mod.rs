use super::super::peer_request::RequestGeneration;
use super::super::pipelined::BlockRequest;
use super::{PeerActorEntry, PeerSwarm};
use crate::engine::bittorrent::peer::connection::BtPeerConn;
use crate::engine::bittorrent::peer::message_handler::peer_scheduler::{
    PeerActorTask, PeerCommand, PeerEvent,
};
use crate::engine::bittorrent::peer::upload_session::{InMemoryPieceProvider, PieceDataProvider};
use aria2_protocol::bittorrent::message::types::{BtMessage, PieceBlockRequest};
use aria2_protocol::bittorrent::peer::connection::PeerConnection;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};

mod events;
mod lifecycle;
mod registry;
