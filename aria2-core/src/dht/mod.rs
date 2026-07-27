//! DHT (Distributed Hash Table) engine for BitTorrent mainline DHT.
//!
//! Implements the Kademlia DHT protocol used by BitTorrent for distributed
//! peer discovery without a central tracker. This module provides the core
//! data structures for the routing table, node management, and token
//! security.
//!
//! # Module Structure
//!
//! - [`constants`] — DHT protocol constants (K=8, timeouts, intervals)
//! - [`node_id`] — 20-byte node identifier with XOR distance metric
//! - [`node`] — DHT node representation (ID, address, health state)
//! - [`bucket`] — K-bucket (8 nodes) with replacement cache
//! - [`bucket_tree`] — Binary tree of K-buckets (Kademlia routing structure)
//! - [`routing_table`] — Top-level routing table API
//! - [`token_tracker`] — Token generation/validation for announce security
//! - [`message`] — DHT message types (ping, find_node, get_peers, announce_peer)
//! - [`message_codec`] — Bencode serialization for DHT messages (BEP 5 encode path)
//! - [`message_decode`] — Bencode deserialization for DHT messages (BEP 5 decode path)
//! - [`tracker`] — Query/response matching by transaction ID with timeout
//! - [`routing_table_ser`] — Routing table persistence (dht.dat load/save)
//! - [`transport`] — UDP socket transport for DHT messages
//! - [`peer_announce`] — Peer announcement storage and lookup
//! - [`task`] — DHT task system (lookup, ping, bucket refresh, node replacement)
//! - [`dispatcher`] — Outbound message queue and send via transport
//! - [`receiver`] — Inbound message receive, decode, and routing table update
//! - [`engine`] — Top-level DHT engine (main loop, periodic tasks, bootstrap)
//!
//! # Architecture
//!
//! The routing table is a binary tree of K-buckets. Each leaf node holds a
//! bucket containing up to K=8 nodes. The tree is split when a bucket
//! overflows and the local node falls within that bucket's range, ensuring
//! the local node's bucket can always accept new nodes.
//!
//! ```text
//!                [root: 0x00..0xFF]
//!               /                  \
//!      [0x00..0x7F]           [0x80..0xFF]
//!      /        \              /          \
//!   [0x00..0x3F] [0x40..0x7F] [0x80..0xBF] [0xC0..0xFF]
//!    (bucket)     (bucket)     (bucket)     (bucket)
//! ```
//!
//! # C++ Reference
//!
//! This implementation follows the aria2 C++ DHT architecture:
//! - `DHTConstants.h` -> [`constants`]
//! - `DHTNode.h/cc` -> [`node`]
//! - `DHTBucket.h/cc` -> [`bucket`]
//! - `DHTBucketTree.h/cc` -> [`bucket_tree`]
//! - `DHTRoutingTable.h/cc` -> [`routing_table`]
//! - `DHTTokenTracker.h/cc` -> [`token_tracker`]
//! - `DHTConnectionImpl.h/cc` -> [`transport`]
//! - `DHTMessageTracker.h/cc` -> [`tracker`]
//! - `DHTPeerAnnounceStorage.h/cc` -> [`peer_announce`]
//! - `DHTTask.h/cc` + `DHTAbstractTask.h/cc` -> [`task`]
//! - `DHTMessageDispatcherImpl.h/cc` -> [`dispatcher`]
//! - `DHTMessageReceiver.h/cc` -> [`receiver`]
//! - `DHTSetup.cc` + `DHTInteractionCommand.cc` + `DHTRegistry.h` -> [`engine`]

pub mod bucket;
pub mod bucket_tree;
pub mod constants;
pub mod dispatcher;
pub mod engine;
pub mod message;
pub mod message_codec;
pub mod message_decode;
pub mod node;
pub mod node_id;
pub mod peer_announce;
pub mod receiver;
pub mod routing_table;
pub mod routing_table_ser;
pub mod task;
pub mod token_tracker;
pub mod tracker;
pub mod transport;

// Re-export primary types for ergonomic access
pub use bucket::DhtBucket;
pub use bucket_tree::BucketTreeNode;
pub use constants::{
    BAD_CONDITION, BUCKET_REFRESH_CHECK_INTERVAL_SECS, BUCKET_REFRESH_INTERVAL_SECS, CACHE_SIZE,
    COMPACT_LEN_IPV4, COMPACT_LEN_IPV6, DHT_MAX_MESSAGE_SIZE, DHT_VERSION, ID_LENGTH, K,
    MAX_ROUTING_TABLE_SIZE, MESSAGE_TIMEOUT_SECS, NODE_CONTACT_INTERVAL_SECS,
    PEER_ANNOUNCE_CHECK_INTERVAL_SECS, PEER_ANNOUNCE_INTERVAL_SECS,
    PEER_ANNOUNCE_PURGE_INTERVAL_SECS, TOKEN_SECRET_COUNT, TOKEN_SECRET_SIZE,
    TOKEN_UPDATE_INTERVAL_SECS, TRANSACTION_ID_LENGTH,
};
pub use dispatcher::DhtDispatcher;
pub use engine::{ActiveLookup, DhtEngine, DhtEngineConfig, DhtEntryPoint};
pub use message::{CompactNodeInfo, CompactPeerInfo, DhtMessage, MessageTypeKind};
pub use message_codec::{MessageCodecError, encode};
pub use message_decode::{decode, decode_response_with_method};
pub use node::DhtNode;
pub use node_id::NodeId;
pub use peer_announce::{AnnouncedPeer, DhtPeerAnnounceStorage, PeerAnnounceEntry};
pub use receiver::{DhtReceiver, ReceiveAction};
pub use routing_table::RoutingTable;
pub use routing_table_ser::{
    DeserializeResult, deserialize_from_file, deserialize_from_reader, serialize_to_file,
    serialize_to_writer,
};
pub use task::{
    DhtBucketRefreshTask, DhtLookupTask, DhtPingTask, DhtReplaceNodeTask, DhtTask, DhtTaskQueue,
    LookupEntry, LookupKind, LookupResult, LookupState, TaskExecutor,
};
pub use token_tracker::TokenTracker;
pub use tracker::{DhtMessageTracker, MatchResult, TimeoutEntry, TrackerEntry};
pub use transport::{AddressFamily, DhtTransport};
