//! DHT protocol constants.
//!
//! Defines the fundamental constants used throughout the DHT implementation,
//! matching the C++ `DHTConstants.h` values. All durations are in seconds
//! for use with `std::time::Duration::from_secs()`.

/// DHT protocol version. Incremented on major improvements or bug fixes.
/// C++: `DHT_VERSION = 3`
pub const DHT_VERSION: u16 = 3;

/// Length of a DHT node ID in bytes (SHA-1 hash size).
/// C++: `DHT_ID_LENGTH = 20`
pub const ID_LENGTH: usize = 20;

/// Length of a DHT transaction ID in bytes.
/// C++: `DHT_TRANSACTION_ID_LENGTH = 4`
pub const TRANSACTION_ID_LENGTH: usize = 4;

/// K-bucket size: maximum number of nodes per bucket (Kademlia K).
/// C++: `DHTBucket::K = 8`
pub const K: usize = 8;

/// Maximum number of cached replacement nodes per bucket.
/// C++: `DHTBucket::CACHE_SIZE = 2`
pub const CACHE_SIZE: usize = 2;

/// Maximum routing table size (number of buckets).
/// C++: `MAX_ROUTING_TABLE_SIZE` (typically 160 for 20-byte IDs)
pub const MAX_ROUTING_TABLE_SIZE: usize = ID_LENGTH * 8;

/// Number of seconds before a DHT message times out.
/// C++: `DHT_MESSAGE_TIMEOUT = 10_s`
pub const MESSAGE_TIMEOUT_SECS: u64 = 10;

/// Interval between node contacts before a node is considered questionable.
/// C++: `DHT_NODE_CONTACT_INTERVAL = 15_min`
pub const NODE_CONTACT_INTERVAL_SECS: u64 = 15 * 60;

/// Interval between full bucket refreshes.
/// C++: `DHT_BUCKET_REFRESH_INTERVAL = 15_min`
pub const BUCKET_REFRESH_INTERVAL_SECS: u64 = 15 * 60;

/// Interval between bucket refresh checks.
/// C++: `DHT_BUCKET_REFRESH_CHECK_INTERVAL = 5_min`
pub const BUCKET_REFRESH_CHECK_INTERVAL_SECS: u64 = 5 * 60;

/// Interval between peer announce purges.
/// C++: `DHT_PEER_ANNOUNCE_PURGE_INTERVAL = 30_min`
pub const PEER_ANNOUNCE_PURGE_INTERVAL_SECS: u64 = 30 * 60;

/// Interval between peer announcements.
/// C++: `DHT_PEER_ANNOUNCE_INTERVAL = 15_min`
pub const PEER_ANNOUNCE_INTERVAL_SECS: u64 = 15 * 60;

/// Interval between peer announce checks.
/// C++: `DHT_PEER_ANNOUNCE_CHECK_INTERVAL = 5_min`
pub const PEER_ANNOUNCE_CHECK_INTERVAL_SECS: u64 = 5 * 60;

/// Interval between token secret rotations.
/// C++: `DHT_TOKEN_UPDATE_INTERVAL = 10_min`
pub const TOKEN_UPDATE_INTERVAL_SECS: u64 = 10 * 60;

/// Condition threshold at which a node is considered "bad".
/// C++: `BAD_CONDITION = 5`
pub const BAD_CONDITION: u32 = 5;

/// Length of compact IP/port info for IPv4 (4 bytes IP + 2 bytes port).
pub const COMPACT_LEN_IPV4: usize = 6;

/// Length of compact IP/port info for IPv6 (16 bytes IP + 2 bytes port).
pub const COMPACT_LEN_IPV6: usize = 18;

/// Size of the token secret in bytes.
/// C++: `DHTTokenTracker::SECRET_SIZE = 4`
pub const TOKEN_SECRET_SIZE: usize = 4;

/// Number of rotating token secrets (current and previous).
pub const TOKEN_SECRET_COUNT: usize = 2;

/// Maximum UDP datagram size for DHT messages in bytes.
/// Per BEP 5, DHT messages must fit in a single UDP datagram of at most 4096 bytes.
/// C++: `DHT_MAX_MESSAGE_SIZE` (defined in DHTConnectionImpl / SocketCore buffer)
pub const DHT_MAX_MESSAGE_SIZE: usize = 4096;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_cpp() {
        assert_eq!(DHT_VERSION, 3);
        assert_eq!(ID_LENGTH, 20);
        assert_eq!(TRANSACTION_ID_LENGTH, 4);
        assert_eq!(K, 8);
        assert_eq!(CACHE_SIZE, 2);
        assert_eq!(MAX_ROUTING_TABLE_SIZE, 160);
        assert_eq!(BAD_CONDITION, 5);
        assert_eq!(TOKEN_SECRET_SIZE, 4);
        assert_eq!(TOKEN_SECRET_COUNT, 2);
        assert_eq!(COMPACT_LEN_IPV4, 6);
        assert_eq!(COMPACT_LEN_IPV6, 18);
        assert_eq!(DHT_MAX_MESSAGE_SIZE, 4096);
    }

    #[test]
    fn duration_values_match_cpp() {
        assert_eq!(MESSAGE_TIMEOUT_SECS, 10);
        assert_eq!(NODE_CONTACT_INTERVAL_SECS, 15 * 60);
        assert_eq!(BUCKET_REFRESH_INTERVAL_SECS, 15 * 60);
        assert_eq!(BUCKET_REFRESH_CHECK_INTERVAL_SECS, 5 * 60);
        assert_eq!(PEER_ANNOUNCE_PURGE_INTERVAL_SECS, 30 * 60);
        assert_eq!(PEER_ANNOUNCE_INTERVAL_SECS, 15 * 60);
        assert_eq!(PEER_ANNOUNCE_CHECK_INTERVAL_SECS, 5 * 60);
        assert_eq!(TOKEN_UPDATE_INTERVAL_SECS, 10 * 60);
    }
}
