//! Constants for peer storage, matching C++ aria2.

/// Maximum number of unused peers to track, matching C++ MAX_PEER_LIST_SIZE.
pub const MAX_PEER_LIST_SIZE: usize = 512;

/// Maximum number of dropped peers to retain for reconnect attempts.
pub const MAX_DROPPED_PEERS: usize = 50;

/// Choke round interval in seconds (matching C++ 10_s).
pub const CHOKE_ROUND_INTERVAL_SECS: u64 = 10;

/// Minimum temporary rejection timeout in seconds (C++ uses 120).
pub const TEMP_REJECT_TIMEOUT_MIN_SECS: u64 = 120;

/// Range of random additional timeout for temporary rejection (C++ uses
/// `getRandomNumber(601)` which returns [0, 600], so total = 120 + [0, 600]
/// = [120, 720]).
pub const TEMP_REJECT_TIMEOUT_RANGE_SECS: u64 = 601;

/// Cleanup interval for expired temporarily-rejected peers (C++ uses 1 hour).
pub const TEMP_PEER_CLEANUP_INTERVAL_SECS: u64 = 3600;
