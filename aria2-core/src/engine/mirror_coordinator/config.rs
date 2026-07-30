// Mirror Coordinator configuration types.

use crate::constants;

/// Configuration for mirror coordination.
#[derive(Debug, Clone)]
pub struct MirrorConfig {
    /// Maximum number of concurrent connections per mirror.
    pub max_connections_per_mirror: usize,
    /// Maximum total concurrent connections across all mirrors.
    pub max_total_connections: usize,
    /// Speed threshold in bytes/sec below which mirrors are deprioritized.
    pub speed_threshold: u64,
    /// Cooldown period in seconds for failed mirrors.
    pub cooldown_secs: u64,
    /// Maximum retries per segment before giving up.
    pub max_retries: u32,
}

impl Default for MirrorConfig {
    fn default() -> Self {
        Self {
            max_connections_per_mirror: constants::DEFAULT_MAX_CONNECTIONS_PER_MIRROR,
            max_total_connections: 16,
            speed_threshold: constants::MIRROR_SPEED_THRESHOLD,
            cooldown_secs: constants::MIRROR_COOLDOWN_SECS,
            max_retries: constants::MAX_MIRROR_FAILURES,
        }
    }
}
