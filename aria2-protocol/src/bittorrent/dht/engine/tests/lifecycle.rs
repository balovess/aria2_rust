use super::super::{DhtEngine, DhtEngineConfig, DhtEngineState};
use crate::bittorrent::dht::node::DhtNode;
use crate::bittorrent::dht::persistence::DhtPersistence;
use crate::bittorrent::dht::store::DhtItemStore;
use crate::bittorrent::dht::task::DhtTask;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[tokio::test]
async fn test_dht_engine_state_subscription_is_event_driven() {
    let engine = DhtEngine::start(DhtEngineConfig::local())
        .await
        .expect("local DHT engine should start");
    let mut states = engine.subscribe_state();

    assert_eq!(*states.borrow(), DhtEngineState::Running);

    engine.shutdown();
    states
        .changed()
        .await
        .expect("state subscription should receive shutdown");
    assert_eq!(*states.borrow(), DhtEngineState::ShuttingDown);

    engine.shutdown_async().await;
}

#[tokio::test]
async fn test_dht_engine_wait_until_ready_observes_bootstrap_transition() {
    let engine = DhtEngine::start(DhtEngineConfig {
        port: 0,
        bootstrap_nodes: vec!["127.0.0.1:9".parse().unwrap()],
        bootstrap_timeout: Duration::from_secs(1),
        ..DhtEngineConfig::default()
    })
    .await
    .expect("DHT engine should bind an ephemeral port");

    engine
        .wait_until_ready(Duration::from_secs(1))
        .await
        .expect("bootstrap lifecycle transition should be observable");
    assert_eq!(engine.state().await, DhtEngineState::Running);

    engine.shutdown_async().await;
}

#[tokio::test]
async fn test_dht_engine_sync_shutdown_is_immediately_observable() {
    let engine = DhtEngine::start(DhtEngineConfig::local())
        .await
        .expect("start should succeed");

    engine.shutdown();

    assert_eq!(engine.state().await, DhtEngineState::ShuttingDown);
    assert_eq!(engine.stats().await.state, DhtEngineState::ShuttingDown);

    engine.shutdown_async().await;
}

#[tokio::test]
async fn cancelled_async_shutdown_retains_background_task_ownership_for_retry() {
    struct TaskLifetime(Arc<AtomicBool>);

    impl Drop for TaskLifetime {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }

    let engine = DhtEngine::start(DhtEngineConfig::local())
        .await
        .expect("local DHT engine should start");
    let task_live = Arc::new(AtomicBool::new(false));
    let task_started = Arc::new(tokio::sync::Notify::new());
    let task_live_guard = Arc::clone(&task_live);
    let task_started_signal = Arc::clone(&task_started);
    let task = async move {
        task_live_guard.store(true, Ordering::Release);
        let _lifetime = TaskLifetime(task_live_guard);
        task_started_signal.notify_one();
        std::future::pending::<()>().await;
    };
    engine.register_background_task(task).await;
    task_started.notified().await;

    let shutdown_engine = Arc::clone(&engine);
    let shutdown = tokio::spawn(async move { shutdown_engine.shutdown_async().await });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if engine.background_tasks.try_lock().is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown should transfer its task set to its join owner");
    assert!(
        !shutdown.is_finished(),
        "the synthetic task should keep shutdown inside its bounded join grace"
    );
    shutdown.abort();
    let _ = shutdown.await;

    assert!(
        task_live.load(Ordering::Acquire),
        "the synthetic background task must still be running before teardown completes"
    );
    tokio::time::timeout(Duration::from_secs(2), engine.shutdown_async())
        .await
        .expect("a retried shutdown should finish");
    assert!(
        !task_live.load(Ordering::Acquire),
        "a cancelled shutdown must not detach background tasks from later teardown"
    );
}

#[tokio::test]
async fn test_dht_engine_tries_next_port_when_first_is_occupied() {
    let occupied = tokio::net::UdpSocket::bind("0.0.0.0:0")
        .await
        .expect("occupied UDP socket");
    let first_port = occupied.local_addr().unwrap().port();
    let candidate = tokio::net::UdpSocket::bind("0.0.0.0:0")
        .await
        .expect("candidate UDP socket");
    let second_port = candidate.local_addr().unwrap().port();
    drop(candidate);

    let config = DhtEngineConfig {
        port: first_port,
        port_range: Some(vec![first_port, second_port]),
        dht_file_path: None,
        ..DhtEngineConfig::local()
    };
    let engine = DhtEngine::start(config)
        .await
        .expect("DHT should fall back to the next available port");

    assert_eq!(
        engine.context.task_context.socket.local_addr().port(),
        second_port
    );
    drop(occupied);
    engine.shutdown_async().await;
}

#[tokio::test]
async fn test_dht_engine_stats() {
    let config = DhtEngineConfig {
        dht_file_path: None,
        ..DhtEngineConfig::local()
    };

    let engine = DhtEngine::start(config)
        .await
        .expect("start should succeed");
    let info_hash_limit = engine.context.peer_storage.stats().max_info_hashes;
    for value in 0..=info_hash_limit {
        let mut info_hash = [0u8; 20];
        info_hash[..8].copy_from_slice(&(value as u64).to_be_bytes());
        engine
            .context
            .peer_storage
            .add_peer(info_hash, "127.0.0.1:6881".parse().unwrap());
    }
    let stats = engine.stats().await;
    assert_eq!(stats.state, DhtEngineState::Running);
    assert_eq!(stats.peer_info_hashes, info_hash_limit);
    assert_eq!(stats.stored_peers, info_hash_limit);
    assert_eq!(stats.peer_storage_evictions, 1);
    assert_eq!(stats.max_peer_info_hashes, info_hash_limit);

    engine.shutdown_async().await;
}

#[tokio::test]
async fn periodic_save_is_queued_while_maintenance_lane_is_busy() {
    #[derive(Debug)]
    struct BlockingTask {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl DhtTask for BlockingTask {
        async fn run(self: Box<Self>) {
            self.started.notify_one();
            self.release.notified().await;
        }

        fn name(&self) -> &'static str {
            "blocking-maintenance-test"
        }
    }

    let temp_dir = tempfile::tempdir().expect("temporary DHT directory should be created");
    let path = temp_dir.path().join("dht.dat");
    let engine = DhtEngine::start(DhtEngineConfig {
        dht_file_path: Some(path.clone()),
        save_interval: Duration::from_millis(150),
        ..DhtEngineConfig::local()
    })
    .await
    .expect("local DHT engine should start");

    tokio::time::timeout(Duration::from_secs(2), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("initial periodic save should create the snapshot");

    let node_id = [0x42; 20];
    engine
        .context
        .task_context
        .routing_table
        .write()
        .await
        .insert(DhtNode::new(node_id, "127.0.0.1:6881".parse().unwrap()));

    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    engine
        .task_queue
        .add_periodic_task_2(Box::new(BlockingTask {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        }))
        .await;
    started.notified().await;

    let persisted_while_busy = tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            if DhtPersistence::load_from_file_sync(&path)
                .is_ok_and(|snapshot| snapshot.nodes.iter().any(|node| node.id == node_id))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .is_ok();

    release.notify_one();
    engine.shutdown_async().await;

    assert!(
        persisted_while_busy,
        "a periodic DHT checkpoint must wait behind busy maintenance instead of being dropped"
    );
}

#[tokio::test]
async fn state_save_attempts_items_when_routing_snapshot_fails() {
    let temp_dir = tempfile::tempdir().expect("temporary DHT directory should be created");
    let path = temp_dir.path().join("dht.dat");
    std::fs::create_dir(&path).expect("routing snapshot path should block file replacement");

    let engine = DhtEngine::start(DhtEngineConfig {
        dht_file_path: Some(path.clone()),
        ..DhtEngineConfig::local()
    })
    .await
    .expect("local DHT engine should start");

    let error = engine
        .save_state()
        .await
        .expect_err("routing snapshot replacement should fail for a directory");
    assert!(error.contains("DHT routing table"));

    let item_path = path.with_extension("items");
    assert!(
        DhtItemStore::load_from_file_sync(&item_path).is_ok(),
        "BEP 44 item store should still be written after routing snapshot failure"
    );

    engine.shutdown_async().await;
}

#[tokio::test]
async fn test_find_peers_when_stopped() {
    // Create an engine in ShuttingDown state
    let config = DhtEngineConfig {
        dht_file_path: None,
        ..DhtEngineConfig::local()
    };

    let engine = DhtEngine::start(config)
        .await
        .expect("start should succeed");
    engine.shutdown();

    // find_peers should return empty when shutting down
    // (may take a moment for state to propagate)
    tokio::time::sleep(Duration::from_millis(200)).await;
    let result = engine.find_peers(&[0u8; 20]).await;
    assert!(result.is_ok());
}
