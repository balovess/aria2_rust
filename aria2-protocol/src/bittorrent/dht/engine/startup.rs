use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::{RwLock, watch};
use tracing::{debug, info, warn};

use super::super::node::DhtNode;
use super::super::peer_storage::DhtPeerStorage;
use super::super::persistence::DhtPersistence;
use super::super::routing_table::RoutingTable;
use super::super::socket::DhtSocket;
use super::super::store::DhtItemStore;
use super::super::task::DhtTaskQueue;
use super::super::task_impl::DhtTaskContext;
use super::super::token_tracker::TokenTracker;
use super::super::tracker::TransactionTracker;
use super::{DhtEngine, DhtEngineConfig, DhtEngineContext, DhtEngineInner, DhtEngineState};

impl DhtEngine {
    /// Start the DHT engine with the given configuration.
    ///
    /// Binds a UDP socket on the configured port, loads the routing table
    /// from disk (if available), spawns the receive loop and periodic tasks,
    /// and returns a shared reference to the running engine.
    ///
    /// Bootstrap into the public DHT network is **not awaited**: it is spawned
    /// as a background task guarded by
    /// [`DhtEngineConfig::bootstrap_timeout`], so `start` returns as soon as
    /// the socket is bound. This matches C++ aria2, where
    /// `DHTEntryPointNameResolveCommand` is queued into the event loop rather
    /// than blocking startup, and it keeps the engine usable (and tests fast)
    /// on hosts with no DHT connectivity.
    ///
    /// Use [`DhtEngine::state`] to observe the transition from
    /// `Bootstrapping` to `Running`, or [`DhtEngineConfig::local`] to skip
    /// bootstrap entirely.
    pub async fn start(config: DhtEngineConfig) -> std::io::Result<Arc<Self>> {
        let persisted_data = if let Some(ref path) = config.dht_file_path
            && tokio::fs::try_exists(path).await.unwrap_or(false)
        {
            match DhtPersistence::load_from_file(path).await {
                Ok(data)
                    if DhtPersistence::is_fresh(data.saved_at_secs, config.persistence_max_age) =>
                {
                    Some(data)
                }
                Ok(data) => {
                    warn!(
                        saved_at = data.saved_at_secs,
                        max_age_secs = config.persistence_max_age.as_secs(),
                        "Ignoring stale DHT routing-table snapshot"
                    );
                    None
                }
                Err(e) => {
                    warn!("Failed to load DHT routing table: {}", e);
                    None
                }
            }
        } else {
            None
        };

        // Prefer an explicitly configured ID, then the persisted local ID,
        // and generate a new ID only when neither is available.
        let self_id = if config.self_id == [0u8; 20]
            && persisted_data
                .as_ref()
                .is_some_and(|data| data.self_id != [0u8; 20])
        {
            persisted_data.as_ref().expect("checked above").self_id
        } else if config.self_id == [0u8; 20] {
            let mut id = [0u8; 20];
            use rand::{RngCore, SeedableRng};
            // Use StdRng instead of ThreadRng to satisfy Send across async boundaries.
            rand::rngs::StdRng::from_entropy().fill_bytes(&mut id);
            id
        } else {
            config.self_id
        };

        let listen_addr = config
            .listen_addr
            .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));

        info!(
            id = %hex::encode(self_id),
            port = config.port,
            ?listen_addr,
            "Starting DHT engine"
        );

        // Bind UDP socket
        let socket = if let Some(ports) = config.port_range.as_deref() {
            let mut last_error = None;
            let mut bound = None;
            for port in ports {
                match DhtSocket::bind_on(SocketAddr::new(listen_addr, *port)).await {
                    Ok(socket) => {
                        bound = Some(socket);
                        break;
                    }
                    Err(error) => last_error = Some(error),
                }
            }
            bound.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    last_error.unwrap_or_else(|| "DHT port range is empty".to_string()),
                )
            })?
        } else {
            DhtSocket::bind_on(SocketAddr::new(listen_addr, config.port))
                .await
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::AddrInUse, e))?
        };
        let actual_port = socket.local_addr().port();
        info!(port = actual_port, "DHT socket bound");

        // Load routing table from disk or start empty
        let mut routing_table = RoutingTable::new(self_id);
        if let Some(data) = persisted_data {
            info!(
                count = data.nodes.len(),
                "Loaded DHT routing table from disk"
            );
            for pnode in data.nodes {
                let node = DhtNode::unverified(pnode.id, pnode.addr);
                routing_table.insert(node);
            }
        }

        let routing_table = Arc::new(RwLock::new(routing_table));
        let inner = Arc::new(RwLock::new(DhtEngineInner {
            state: DhtEngineState::Bootstrapping,
        }));
        let (state_updates, _state_receiver) = watch::channel(DhtEngineState::Bootstrapping);

        let token_tracker = Arc::new(std::sync::Mutex::new(TokenTracker::new()));
        let peer_storage = Arc::new(DhtPeerStorage::new());
        let item_store = config
            .dht_file_path
            .as_deref()
            .map(|path| path.with_extension("items"))
            .and_then(|path| match DhtItemStore::load_from_file_sync(&path) {
                Ok(store) => {
                    info!(path = %path.display(), "Loaded BEP 44 item store");
                    Some(store)
                }
                Err(error) => {
                    debug!(path = %path.display(), "BEP 44 item store unavailable: {error}");
                    None
                }
            })
            .unwrap_or_default();
        let tracker = Arc::new(TransactionTracker::new());
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let routing_table_save_lock = Arc::new(tokio::sync::Mutex::new(()));
        let task_context = DhtTaskContext::new(
            self_id,
            Arc::clone(&routing_table),
            socket.clone(),
            Arc::clone(&tracker),
            config.query_timeout,
        );
        let task_queue = Arc::new(DhtTaskQueue::with_concurrency(
            config.max_concurrent_lookups,
        ));
        let engine_context = Arc::new(DhtEngineContext {
            inner: Arc::clone(&inner),
            state_updates,
            routing_table_save_lock,
            config: config.clone(),
            token_tracker: Arc::clone(&token_tracker),
            peer_storage: Arc::clone(&peer_storage),
            item_store,
            shutdown_requested: Arc::clone(&shutdown_requested),
            task_context,
        });

        let engine = Arc::new(Self {
            context: engine_context,
            shutdown_tx,
            background_tasks: std::sync::Mutex::new(Vec::new()),
            task_queue,
        });

        // Spawn the background receive loop
        engine.spawn_receive_loop(shutdown_rx);

        // Spawn periodic tasks
        engine.spawn_periodic_tasks();

        // Bootstrap runs in the background so `start` never blocks on network
        // I/O. Without a bootstrap the engine is immediately usable for
        // inbound queries and for peers added manually (e.g. from a torrent's
        // `nodes` list), so we move straight to `Running`.
        if config.bootstrap_on_start {
            engine.spawn_bootstrap();
        } else {
            engine.context.inner.write().await.state = DhtEngineState::Running;
            let _ = engine.context.state_updates.send(DhtEngineState::Running);
        }

        Ok(engine)
    }
}
