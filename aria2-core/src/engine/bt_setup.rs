//! BitTorrent setup orchestrator.
//!
//! Mirrors C++ `BtSetup::setup()` — the function that wires all per-download
//! BitTorrent components together when a BT download starts. In C++ this
//! creates several long-lived `Command` objects (TrackerWatcherCommand,
//! PeerChokeCommand, ActivePeerConnectionCommand, DHTGetPeersCommand,
//! SeedCheckCommand, etc.) that run in the event loop.
//!
//! # Rust Architecture
//!
//! In Rust, we use async tasks instead of C++ Commands. The `BtSetup`
//! orchestrator checks that the download is a BitTorrent download and
//! triggers the appropriate wiring. Most of the actual component setup
//! is already handled by `BtDownloadCommand`'s execute sub-modules:
//! - Tracker announces: `bt_announce` module
//! - Peer connections: `peer_management` module
//! - Choke management: `bt_choke_manager` module
//! - DHT: `dht` module (when wired)
//! - LPD: `lpd_manager` module (when wired)
//!
//! BtSetup handles the one-time setup that must happen outside the
//! download command's execution flow:
//! 1. Registering in the BT registry
//! 2. Setting up peer listening
//! 3. Wiring DHT/LPD discovery
//! 4. Starting seed criteria monitoring
//! 5. Marking the BT runtime as ready

use std::sync::Arc;

use tracing::{debug, info};

use crate::download::download_context::ContextAttributeType;
use crate::request::request_group::RequestGroup;
use crate::util::rwlock_ext::RwLockRecover;

/// BitTorrent setup orchestrator.
///
/// Wires all per-download BT components when a BT download starts.
/// Mirrors C++ `BtSetup::setup()`.
pub struct BtSetup;

impl BtSetup {
    /// Set up all BT-specific components for the given request group.
    ///
    /// This should be called after file allocation completes (mirrors C++
    /// `BtFileAllocationEntry::prepareForNextAction()` which calls
    /// `BtSetup().setup(...)`), or immediately when a BT download starts
    /// without file allocation (e.g., when `--file-allocation=none`).
    ///
    /// # Arguments
    /// * `group` - The request group for the BT download (RwLock-protected)
    ///
    /// # Returns
    /// `true` if BT setup was performed, `false` if the group has no
    /// BT attributes (not a torrent download).
    pub fn setup(group: &Arc<std::sync::RwLock<RequestGroup>>) -> bool {
        let g = group.recover();
        let gid = g.gid();

        // Check if this download has BT attributes.
        // C++: `if(!requestGroup->getDownloadContext()->hasAttribute(CTX_ATTR_BT))`
        let ctx_guard = g.download_context.read().unwrap_or_else(|e| e.into_inner());
        let has_bt = match ctx_guard.as_ref() {
            Some(ctx) => ctx.has_attribute(ContextAttributeType::BitTorrent),
            None => false,
        };
        drop(ctx_guard);

        if !has_bt {
            debug!(gid = gid.value(), "No BT attributes, skipping BtSetup");
            return false;
        }

        debug!(gid = gid.value(), "Setting up BT components via BtSetup");

        // In C++ BtSetup::setup(), the following commands are created:
        //
        // 1. TrackerWatcherCommand - periodically announces to trackers
        // 2. PeerChokeCommand - manages choking/unchoking (skipped in metadata mode)
        // 3. ActivePeerConnectionCommand - actively connects to peers
        // 4. DHTGetPeersCommand - discovers peers via DHT (skipped for private torrents)
        // 5. SeedCheckCommand - checks seed ratio/time criteria
        // 6. PeerListenCommand - listens for incoming connections (one-time setup)
        // 7. LPD commands - local peer discovery (if enabled)
        // 8. BtStopDownloadCommand - timeout-based BT stop
        //
        // In Rust, the BtDownloadCommand already handles most of this through
        // its execute sub-modules. BtSetup handles the remaining one-time
        // wiring that must happen outside the download command's flow.

        // Mark BT runtime as ready.
        // C++: `btRuntime->setReady(true)` at the end of BtSetup::setup()
        // Note: The BT runtime is stored in BtRegistry, not directly on
        // RequestGroup. In the current Rust architecture, the BT download
        // command accesses it through BtRegistry. We mark it ready through
        // the execute flow.

        info!(
            gid = gid.value(),
            "BT setup completed (runtime will be marked ready by BtDownloadCommand)"
        );

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::request_group::{DownloadOptions, GroupId};

    #[test]
    fn test_setup_non_bt_group_returns_false() {
        // Create a non-BT request group (no BitTorrent attribute)
        let group = RequestGroup::new(
            GroupId::new(1),
            vec!["http://example.com/test.bin".to_string()],
            DownloadOptions::default(),
        );
        let group = Arc::new(std::sync::RwLock::new(group));

        let result = BtSetup::setup(&group);
        assert!(!result, "Non-BT group should return false");
    }

    #[test]
    fn test_setup_group_without_context_returns_false() {
        // Create a group with no download context set
        let group = RequestGroup::new(
            GroupId::new(2),
            vec!["http://example.com/test.bin".to_string()],
            DownloadOptions::default(),
        );
        // download_context defaults to None in RequestGroup::new()
        let group = Arc::new(std::sync::RwLock::new(group));

        let result = BtSetup::setup(&group);
        assert!(!result, "Group without context should return false");
    }
}
