//! Tests for the BitTorrent peer connection module.

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use super::peer_conn::{BtPeerConn, KEEPALIVE_INTERVAL_SECS, PEER_TIMEOUT_SECS};
use super::session_resource::PeerSessionResource;
use super::types::SendBuffer;

#[path = "tests/connection_state.rs"]
mod connection_state;
#[path = "tests/extensions.rs"]
mod extensions;
#[path = "tests/session_resources.rs"]
mod session_resources;
