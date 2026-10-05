//! Torrent-scoped ownership and coordination for long-lived peer actors.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use super::PeerEvent;
use crate::engine::bittorrent::peer::connection::PeerActorId;
use crate::engine::bittorrent::peer::stats::SwarmUploadRate;

mod entry;
mod event_lease;
mod events;
mod lifecycle;
mod snapshots;

#[cfg(test)]
mod tests;

pub(crate) use entry::PeerActorEntry;
pub(crate) use event_lease::PeerSwarmEventLease;

const PEER_STATS_SNAPSHOT_MIN_INTERVAL: Duration = Duration::from_millis(250);
const MAX_RECENTLY_DROPPED_PEERS: usize = 50;
const MAX_KNOWN_SEEDER_ENDPOINTS: usize = 1024;

/// Torrent-scoped owner for peer actors and their bounded I/O event stream.
#[derive(Default)]
pub(crate) struct PeerSwarm {
    actors: Vec<PeerActorEntry>,
    indices: HashMap<PeerActorId, usize>,
    peer_id_counts: HashMap<[u8; 20], usize>,
    endpoint_counts: HashMap<SocketAddr, usize>,
    recently_dropped_endpoints: VecDeque<(SocketAddr, Instant)>,
    known_seeders: HashSet<SocketAddr>,
    known_seeder_order: VecDeque<SocketAddr>,
    wanted_pieces: Arc<[u8]>,
    local_seeder: bool,
    local_metadata: Option<Arc<[u8]>>,
    peer_snapshot_store:
        Option<Arc<std::sync::RwLock<Vec<crate::request::request_group::BtPeerSnapshot>>>>,
    last_stats_snapshot_publish: Option<Instant>,
    stats_snapshot_dirty: bool,
    upload_rate: Arc<SwarmUploadRate>,
    pub(crate) event_tx: Option<mpsc::Sender<PeerEvent>>,
    pub(crate) event_rx: Option<mpsc::Receiver<PeerEvent>>,
}
