//! LPD Receive Loop — background task for continuous Local Peer Discovery
//!
//! This module implements the background receive loop for Local Peer
//! Discovery (LPD, BEP 14). The loop continuously reads LPD multicast
//! announcements from the UDP socket and feeds discovered peers back
//! to the [`LpdManager`](super::lpd_manager::LpdManager).
//!
//! # Architecture
//!
//! - [`LpdReceiveLoop`] — Manages the background tokio task that
//!   continuously receives LPD announcements. Mirrors C++
//!   `LpdReceiveMessageCommand` which re-adds itself to the event
//!   loop after each receive.
//!
//! # C++ Equivalence
//!
//! | Rust | C++ |
//! |---|---|
//! | `LpdReceiveLoop` | `LpdReceiveMessageCommand` |
//! | `run_receive_loop()` | `LpdReceiveMessageCommand::execute()` loop |

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

// ===========================================================================
// LpdReceiveLoop — background LPD receive task manager
// ===========================================================================

/// Manages the background tokio task that continuously receives LPD
/// multicast announcements.
///
/// The receive loop:
/// 1. Binds to the LPD multicast group (239.192.152.143:6771)
/// 2. Continuously reads LPD announcement messages
/// 3. Parses peer info (info_hash, port) from each message
/// 4. Feeds discovered peers to the [`LpdManager`](super::lpd_manager::LpdManager)
///
/// Mirrors C++ `LpdReceiveMessageCommand` which re-adds itself
/// to the event loop after each receive.
#[derive(Debug)]
pub struct LpdReceiveLoop {
    /// Handle to the background receive task
    _task_handle: Option<tokio::task::JoinHandle<()>>,
    /// Cancellation token to gracefully stop the receive loop
    cancel_token: CancellationToken,
    /// Whether the receive loop is currently running
    is_running: bool,
}

impl LpdReceiveLoop {
    /// Create a new receive loop in a stopped state.
    ///
    /// Call `start()` to begin receiving LPD announcements.
    pub fn new() -> Self {
        Self {
            _task_handle: None,
            cancel_token: CancellationToken::new(),
            is_running: false,
        }
    }

    /// Start the background receive loop.
    ///
    /// If already running, this is a no-op.
    pub async fn start(&mut self) {
        if self.is_running {
            debug!("LPD receive loop already running");
            return;
        }

        // TODO: Implement the actual receive loop that:
        // 1. Binds to the LPD multicast address (239.192.152.143:6771)
        // 2. Joins the multicast group
        // 3. Continuously reads UDP datagrams
        // 4. Parses BEP 14 LPD announcement messages
        // 5. Feeds discovered peers back to LpdManager

        info!("LPD receive loop started");
        self.is_running = true;
    }

    /// Stop the background receive loop gracefully.
    ///
    /// Cancels the background task and waits for it to finish.
    pub async fn stop(&mut self) {
        if !self.is_running {
            return;
        }

        self.cancel_token.cancel();

        if let Some(handle) = self._task_handle.take() {
            // Wait for the task to finish (with timeout)
            match tokio::time::timeout(Duration::from_secs(5), handle).await {
                Ok(Ok(())) => {
                    info!("LPD receive loop stopped gracefully");
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "LPD receive loop task panicked");
                }
                Err(_) => {
                    warn!("LPD receive loop stop timed out after 5s");
                }
            }
        }

        self.is_running = false;
        // Create a fresh cancellation token for potential restart
        self.cancel_token = CancellationToken::new();
    }

    /// Check if the receive loop is currently running.
    pub fn is_running(&self) -> bool {
        self.is_running
    }
}

impl Default for LpdReceiveLoop {
    fn default() -> Self {
        Self::new()
    }
}
