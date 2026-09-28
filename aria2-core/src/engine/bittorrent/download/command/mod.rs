mod choke_api;
mod constructor;
mod integration_api;

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::engine::bittorrent::dht::engine_set::DhtEngineSet;
use crate::engine::bittorrent::discovery::lpd::LpdManager;
use crate::engine::bittorrent::peer::choking_algorithm::ChokingAlgorithm;
use crate::engine::bittorrent::torrent::file_layout::MultiFileLayout;
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{AtomicProgress, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

// Re-export sub-module public items
pub use constructor::prepare_group_metadata;
pub(crate) use constructor::{
    apply_file_mappings, apply_select_file_filter, build_download_context_from_meta,
};
pub(crate) const MAX_PUBLIC_TRACKERS_TO_TRY: usize = 10;

#[derive(Debug)]
pub(crate) struct BtRuntimeState {
    connections: std::sync::atomic::AtomicUsize,
    max_peers: std::sync::atomic::AtomicUsize,
}

impl BtRuntimeState {
    pub(crate) fn new(max_peers: usize) -> Self {
        Self {
            connections: std::sync::atomic::AtomicUsize::new(0),
            max_peers: std::sync::atomic::AtomicUsize::new(max_peers),
        }
    }

    pub(crate) fn set_connections(&self, connections: usize) {
        self.connections
            .store(connections, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn set_max_peers(&self, max_peers: usize) {
        self.max_peers
            .store(max_peers, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn connections(&self) -> usize {
        self.connections.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn max_peers(&self) -> usize {
        self.max_peers.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn min_peers(&self) -> usize {
        let max_peers = self.max_peers.load(std::sync::atomic::Ordering::Acquire);
        if max_peers == 0 {
            0
        } else {
            (max_peers * 4 / 5).max(1)
        }
    }

    pub(crate) fn less_than_min_peers(&self) -> bool {
        self.connections() < self.min_peers()
    }

    pub(crate) fn less_than_max_peers(&self) -> bool {
        self.max_peers() == 0 || self.connections() < self.max_peers()
    }
}

impl Drop for BtDownloadCommand {
    fn drop(&mut self) {
        self.bt_peer_route.take();

        if let Some(registry) = self.bt_registry.as_ref()
            && let Ok(mut registry) = registry.write()
        {
            for engine in self.dht_engines.iter() {
                registry.clear_dht_engine_for_gid_if(self.group.recover().gid().value(), engine);
            }
            let gid = self.group.recover().gid().value();
            let owns_registration = self.tracker_runtime.as_ref().is_some_and(|runtime| {
                registry
                    .get(gid)
                    .and_then(|object| object.tracker_runtime.as_ref())
                    .is_some_and(|registered| Arc::ptr_eq(registered, runtime))
            });
            if owns_registration {
                registry.remove(gid);
            }
        }

        let mut storage = self
            .peer_storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let peers: Vec<_> = storage.used_peers().iter().cloned().collect();
        for peer in peers {
            storage.return_peer(&peer);
        }
    }
}

pub struct BtDownloadCommand {
    /// Stable BitTorrent peer ID for this download session.
    pub(crate) local_peer_id: [u8; 20],
    pub(crate) group: Arc<std::sync::RwLock<RequestGroup>>,
    /// Direct access to progress counters -- avoids RwLock on the hot path.
    pub(crate) progress: Arc<AtomicProgress>,
    pub(crate) output_path: std::path::PathBuf,
    pub(crate) started: bool,
    /// Monotonic timestamp captured when execution begins.
    pub(crate) started_at: Option<Instant>,
    pub(crate) completed_bytes: u64,
    pub(crate) torrent_data: Vec<u8>,
    pub(crate) seed_enabled: bool,
    pub(crate) seed_time: Option<std::time::Duration>,
    pub(crate) seed_ratio: Option<f64>,
    pub(crate) total_uploaded: u64,
    /// Torrent-scoped actor owns the tracker announce state and deadlines.
    pub(crate) tracker_actor:
        Option<crate::engine::bittorrent::download::execute::BtTrackerAnnouncerActor>,
    /// Actual TCP listener port advertised to trackers for this command.
    pub(crate) listen_port: u16,
    pub(crate) bt_runtime: std::sync::Arc<BtRuntimeState>,
    pub(crate) peer_coordinator: crate::engine::bittorrent::peer::coordinator::BtPeerCoordinator,
    pub(crate) dht_engines: DhtEngineSet,
    pub(crate) public_trackers:
        Option<std::sync::Arc<aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList>>,
    pub(crate) choking_algo: Option<ChokingAlgorithm>,
    pub(crate) multi_file_layout: Option<MultiFileLayout>,

    /// File allocation strategy from options
    /// ("none" / "prealloc" / "falloc" / "trunc" / "mmap"). Mirrors C++
    /// `FileAllocationEntry` choosing an iterator from `PREF_FILE_ALLOCATION`.
    pub(crate) file_allocation: String,
    /// Zero-fill after fallocate on platforms that don't zero-fill.
    pub(crate) secure_falloc: bool,
    /// `--check-integrity`: verify existing data against piece hashes before
    /// downloading (C++ `CheckIntegrityMan`).
    pub(crate) check_integrity: bool,
    /// Only perform the piece hash check and terminate without peer discovery.
    pub(crate) hash_check_only: bool,
    /// Allow the BitTorrent completion hook/notification when an existing
    /// payload passes `check-integrity`.
    pub(crate) bt_enable_hook_after_hash_check: bool,
    /// Continue into the BitTorrent peer/seed lifecycle after a complete
    /// payload passes `check-integrity`.
    pub(crate) bt_hash_check_seed: bool,
    /// Treat an existing payload as complete without verifying piece hashes.
    pub(crate) bt_seed_unverified: bool,
    /// Whether the current command completed from an integrity check rather
    /// than by downloading missing pieces.
    pub(crate) hash_check_completed: bool,
    /// Whether the BT completion event was already emitted at the integrity
    /// check seam.
    pub(crate) bt_complete_event_emitted: bool,

    // Optional integrations owned by the download lifecycle.
    /// BT progress persistence manager
    pub(crate) progress_manager:
        Option<crate::engine::bittorrent::persistence::progress_info_file::BtProgressManager>,
    /// Progress save interval (default 60 seconds)
    pub(crate) progress_save_interval: Duration,
    /// LPD LAN peer discovery manager
    pub(crate) lpd_manager: Option<Arc<LpdManager>>,
    /// Info-hash registered in the shared LPD manager for this command.
    pub(crate) lpd_registered_info_hash: Option<[u8; 20]>,
    /// Post-download handler manager
    pub(crate) hook_manager: Option<Arc<crate::engine::hook_manager::HookManager>>,

    // PEX (Peer Exchange, BEP 11) integration fields
    /// Track known peers for PEX exchange
    pub(crate) pex_known_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    /// Timestamp of last PEX message sent (for rate limiting)
    pub(crate) pex_last_send_time: Option<Instant>,
    /// Interval between PEX messages (default 60 seconds)
    pub(crate) pex_send_interval: Duration,

    // Periodic DHT peer lookup (C++ DHTGetPeersCommand)
    /// Tracks timing and retry state for periodic DHT get_peers lookups.
    /// C++: DHTGetPeersCommand runs as a per-torrent command that
    /// triggers DHT lookups at adaptive intervals (15min normal,
    /// 5min low peers, 1min zero peers, 5s retry).
    /// Periodic lookup completion is consumed at BT piece-loop scheduling
    /// boundaries after the background task publishes an event.
    pub(crate) dht_periodic_lookup: super::execute::DhtPeriodicLookup,

    // File lock (J6): prevents concurrent aria2 instances from writing to same output dir
    /// Download path lock held for the lifetime of this command.
    /// Prevents other aria2 instances from writing to the same output directory.
    #[allow(dead_code)]
    pub(crate) download_path_lock: Option<crate::filesystem::file_lock::DownloadPathLock>,

    // BEP 0027 (Private Torrent): when true, DHT/PEX/LPD and public tracker
    // announcement are disabled to enforce the privacy guarantees of the
    // torrent private flag.
    pub(crate) is_private: bool,

    // BtRegistry integration: the command registers itself into the engine BtRegistry
    // during execute() so that info-hash reverse lookup, peer
    // blocklist, and cross-download coordination work end-to-end.
    // Set via set_bt_registry() after construction by the engine or caller.
    pub(crate) bt_registry: Option<Arc<std::sync::RwLock<super::super::registry::BtRegistry>>>,
    /// Shared live tracker state published to the engine registry for RPC.
    pub(crate) tracker_runtime:
        Option<crate::engine::bittorrent::tracker::communication::SharedTrackerRuntime>,

    /// Process-wide rate limiter from `DownloadEngine::global_limiter`.
    /// When `Some`, passed down to `ThrottledWriter` so that this torrent's
    /// piece writes share a single bandwidth ceiling with all concurrent
    /// downloads.
    pub(crate) global_limiter: Option<RateLimiter>,
    /// Torrent-scoped upload limiter shared by every peer actor. The request
    /// group retains the same handle so RPC option updates affect live peers.
    pub(crate) torrent_upload_limiter: RateLimiter,
    /// Process-wide outbound TCP policy for tracker/peer protocol adapters.
    pub(crate) outbound_network_policy: Arc<crate::network::OutboundNetworkPolicy>,

    /// Shared rejection state for verified bad piece sources.
    pub(crate) peer_rejection: crate::engine::bittorrent::peer::storage::SharedPeerRejection,

    /// Session-scoped peer identity pool shared by discovery and connection
    /// scheduling. Socket ownership remains in the download loop until the
    /// lifecycle adapter is wired in.
    pub(crate) peer_storage: std::sync::Arc<
        std::sync::Mutex<crate::engine::bittorrent::peer::storage::DefaultPeerStorage>,
    >,

    /// Receiver for incoming peers routed by the engine-owned listener.
    pub(crate) incoming_peers:
        Option<crate::engine::bittorrent::peer::listener::IncomingPeerReceiver>,
    /// Shared uTP socket for outbound peers in this download task.
    pub(crate) utp_socket:
        Option<Arc<tokio::sync::Mutex<aria2_protocol::bittorrent::utp::UtpSocket>>>,
    /// Process-level listener shared by all BitTorrent downloads.
    pub(crate) bt_listener:
        Option<Arc<crate::engine::bittorrent::peer::listener::BtPeerListenerManager>>,
    /// RAII registration for this torrent's info-hash route.
    pub(crate) bt_peer_route: Option<crate::engine::bittorrent::peer::listener::BtPeerRouteHandle>,
    /// Project extension state for verified torrent pieces.
    pub(crate) checkpoint: Option<crate::engine::bittorrent::persistence::checkpoint::BtCheckpoint>,
    /// Bytes verified since the last durable torrent checkpoint.
    pub(crate) checkpoint_bytes_since_save: u64,
    /// Time at which the last durable torrent checkpoint completed.
    pub(crate) checkpoint_last_save: Instant,
}

impl BtDownloadCommand {
    pub fn group(&self) -> std::sync::RwLockReadGuard<'_, RequestGroup> {
        use crate::util::rwlock_ext::RwLockRecover;
        self.group.recover()
    }

    pub fn group_handle(&self) -> Arc<std::sync::RwLock<RequestGroup>> {
        Arc::clone(&self.group)
    }

    /// Complete the asynchronous shutdown phase before the command is dropped.
    ///
    /// `Drop` can only reclaim synchronous resources. Callers that own the
    /// command lifecycle should await this method before aborting or dropping
    /// the task so tracker stopped announcements and DHT routing-table
    /// persistence are not lost.
    pub async fn shutdown(&mut self) {
        // The DHT engine owns background receive/maintenance tasks and its
        // final routing-table snapshot. Shut it down before the command is
        // dropped; DhtEngine::Drop only aborts tasks and cannot persist state.
        let engines = std::mem::take(&mut self.dht_engines).into_vec();
        if let Some(registry) = self.bt_registry.as_ref()
            && let Ok(mut registry) = registry.write()
        {
            for engine in &engines {
                registry.clear_dht_engine_for_gid_if(self.group.recover().gid().value(), engine);
            }
        }
        for engine in engines {
            let is_global = self
                .bt_registry
                .as_ref()
                .and_then(|registry| registry.read().ok())
                .is_some_and(|registry| registry.is_global_dht_engine(&engine));
            if !is_global {
                engine.shutdown_async().await;
            }
        }
        if let (Some(manager), Some(info_hash)) =
            (&self.lpd_manager, self.lpd_registered_info_hash.take())
        {
            let info_hash_hex = hex::encode(info_hash);
            manager.unregister_torrent(&info_hash_hex).await;
        }
        if let Some(actor) = self.tracker_actor.take() {
            let _ = actor.stop().await;
        }
        self.bt_peer_route.take();
    }

    /// Set the process-wide rate limiter (from `DownloadEngine::global_limiter`).
    ///
    /// When set, piece writes performed by this command acquire tokens from
    /// this limiter (in addition to any per-download limiter) so that all
    /// concurrent downloads share a global bandwidth ceiling.
    pub fn set_global_limiter(&mut self, limiter: RateLimiter) {
        self.global_limiter = Some(limiter);
    }

    pub fn is_multi_file(&self) -> bool {
        self.multi_file_layout
            .as_ref()
            .is_some_and(|l| l.is_multi_file())
    }
}

#[cfg(test)]
mod tests {
    use super::{BtDownloadCommand, BtRuntimeState};

    #[tokio::test]
    async fn shutdown_persists_owned_dht_engine_before_drop() {
        let torrent = crate::engine::bittorrent::download::command_tests::build_test_torrent();
        let options = crate::request::request_group::DownloadOptions::default();
        let mut command = BtDownloadCommand::new(
            crate::request::request_group::GroupId::new(9),
            &torrent,
            &options,
            None,
        )
        .expect("test torrent should construct");

        let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
        let dht_path = temp_dir.path().join("dht.dat");
        let dht = aria2_protocol::bittorrent::dht::engine::DhtEngine::start(
            aria2_protocol::bittorrent::dht::engine::DhtEngineConfig {
                self_id: [0xA5; 20],
                dht_file_path: Some(dht_path.clone()),
                ..aria2_protocol::bittorrent::dht::engine::DhtEngineConfig::local()
            },
        )
        .await
        .expect("local DHT engine should start");
        command.dht_engines.insert(dht);

        command.shutdown().await;

        let persisted =
            aria2_protocol::bittorrent::dht::persistence::DhtPersistence::load_from_file_sync(
                &dht_path,
            )
            .expect("command shutdown should persist dht.dat");
        assert_eq!(persisted.self_id, [0xA5; 20]);
    }

    #[tokio::test]
    async fn task_shutdown_keeps_the_shared_dht_engine_alive() {
        let torrent = crate::engine::bittorrent::download::command_tests::build_test_torrent();
        let options = crate::request::request_group::DownloadOptions::default();
        let mut command = BtDownloadCommand::new(
            crate::request::request_group::GroupId::new(778),
            &torrent,
            &options,
            None,
        )
        .expect("test torrent should construct");
        let registry = std::sync::Arc::new(std::sync::RwLock::new(
            crate::engine::bittorrent::registry::BtRegistry::new(),
        ));
        command.set_bt_registry(std::sync::Arc::clone(&registry));
        let dht = aria2_protocol::bittorrent::dht::engine::DhtEngine::start(
            aria2_protocol::bittorrent::dht::engine::DhtEngineConfig::local(),
        )
        .await
        .expect("local DHT engine should start");
        command.dht_engines.insert(std::sync::Arc::clone(&dht));
        registry
            .write()
            .expect("BT registry should be writable")
            .set_global_dht_engine(std::sync::Arc::clone(&dht));

        command.shutdown().await;

        assert!(!matches!(
            dht.stats().await.state,
            aria2_protocol::bittorrent::dht::engine::DhtEngineState::Stopped
        ));
        dht.shutdown_async().await;
    }

    #[test]
    fn runtime_state_uses_the_same_min_peer_boundary_as_tracker_demand() {
        let runtime = BtRuntimeState::new(55);
        assert_eq!(runtime.min_peers(), 44);
        assert!(runtime.less_than_min_peers());

        runtime.set_connections(44);
        assert!(!runtime.less_than_min_peers());

        runtime.set_max_peers(0);
        assert_eq!(runtime.min_peers(), 0);
        assert!(!runtime.less_than_min_peers());
        assert!(runtime.less_than_max_peers());
    }

    #[test]
    fn runtime_state_accepts_runtime_max_peer_changes() {
        let runtime = BtRuntimeState::new(10);
        runtime.set_connections(7);
        assert!(runtime.less_than_min_peers());

        runtime.set_max_peers(8);
        assert!(!runtime.less_than_min_peers());
        assert_eq!(runtime.max_peers(), 8);
    }

    #[test]
    fn explicit_zero_seed_time_overrides_the_default_seed_ratio() {
        let torrent = crate::engine::bittorrent::download::command_tests::build_test_torrent();
        let options = crate::request::request_group::DownloadOptions {
            seed_time: Some(0.0),
            ..Default::default()
        };
        let command = BtDownloadCommand::new(
            crate::request::request_group::GroupId::new(10),
            &torrent,
            &options,
            None,
        )
        .expect("test torrent should construct");

        assert!(!command.seed_enabled);
    }

    #[test]
    fn positive_seed_options_enable_seeding() {
        let torrent = crate::engine::bittorrent::download::command_tests::build_test_torrent();
        let options = crate::request::request_group::DownloadOptions {
            seed_time: Some(1.0),
            ..Default::default()
        };
        let command = BtDownloadCommand::new(
            crate::request::request_group::GroupId::new(11),
            &torrent,
            &options,
            None,
        )
        .expect("test torrent should construct");

        assert!(command.seed_enabled);
        assert_eq!(
            command.seed_time,
            Some(std::time::Duration::from_secs(60)),
            "seed-time is expressed in fractional minutes"
        );
    }

    #[test]
    fn command_uses_configured_peer_id_prefix() {
        let torrent = crate::engine::bittorrent::download::command_tests::build_test_torrent();
        let options = crate::request::request_group::DownloadOptions {
            peer_id_prefix: "TEST-PREFIX-".to_string(),
            ..Default::default()
        };
        let command = BtDownloadCommand::new(
            crate::request::request_group::GroupId::new(12),
            &torrent,
            &options,
            None,
        )
        .expect("test torrent should construct");

        assert!(command.local_peer_id.starts_with(b"TEST-PREFIX-"));
    }

    #[test]
    fn command_loads_configured_peer_blocklist_into_peer_storage() {
        let torrent = crate::engine::bittorrent::download::command_tests::build_test_torrent();
        let path = std::env::temp_dir().join(format!(
            "aria2-rust-command-blocklist-{}.txt",
            std::process::id()
        ));
        std::fs::write(&path, "10.0.0.0/8\n").expect("blocklist fixture should be writable");
        let options = crate::request::request_group::DownloadOptions {
            bt_peer_blocklist: Some(path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let command = BtDownloadCommand::new(
            crate::request::request_group::GroupId::new(13),
            &torrent,
            &options,
            None,
        )
        .expect("test torrent should construct");

        let blocked =
            crate::engine::bittorrent::peer::storage::PeerEntry::new("10.0.0.1".into(), 6881);
        let allowed =
            crate::engine::bittorrent::peer::storage::PeerEntry::new("192.0.2.1".into(), 6881);
        let mut storage = command.peer_storage.lock().unwrap();
        assert!(!storage.add_peer(blocked));
        assert!(storage.add_peer(allowed));
        drop(storage);
        let _ = std::fs::remove_file(path);
    }
}
