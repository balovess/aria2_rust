use super::*;
use crate::request::request_group::{DownloadOptions, GroupId};

fn add_command(gid: u64) -> EngineCommand {
    EngineCommand::AddDownload {
        group: Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(gid),
            vec![format!("http://example.com/{gid}.bin")],
            DownloadOptions::default(),
        ))),
    }
}

#[test]
fn control_commands_are_drained_before_normal_commands() {
    let (sender, mut receiver) = channel();
    sender.send(add_command(1)).unwrap();
    sender
        .send(EngineCommand::Pause {
            gid: GroupId::new(1),
        })
        .unwrap();

    assert!(matches!(
        receiver.try_recv(),
        Ok(EngineCommand::Pause { .. })
    ));
    assert!(matches!(
        receiver.try_recv(),
        Ok(EngineCommand::AddDownload { .. })
    ));
}

#[cfg(feature = "bittorrent")]
#[test]
fn dht_runtime_limits_are_drained_before_normal_commands() {
    let (sender, mut receiver) = channel();
    sender.send(add_command(1)).unwrap();
    sender
        .send(EngineCommand::SetDhtPeerStorageConfig {
            config: aria2_protocol::bittorrent::dht::DhtPeerStorageConfig {
                max_info_hashes: 128,
                max_peers_per_info_hash: 24,
            },
        })
        .unwrap();
    sender
        .send(EngineCommand::SetDhtItemStoreLimit { max_items: 512 })
        .unwrap();

    assert!(matches!(
        receiver.try_recv(),
        Ok(EngineCommand::SetDhtPeerStorageConfig { .. })
    ));
    assert!(matches!(
        receiver.try_recv(),
        Ok(EngineCommand::SetDhtItemStoreLimit { max_items: 512 })
    ));
    assert!(matches!(
        receiver.try_recv(),
        Ok(EngineCommand::AddDownload { .. })
    ));
}

#[test]
fn duplicate_gid_controls_are_coalesced_until_dequeue() {
    let (sender, mut receiver) = channel();
    let pause = || EngineCommand::Pause {
        gid: GroupId::new(7),
    };

    sender.send(pause()).unwrap();
    sender.send(pause()).unwrap();

    let snapshot = sender.snapshot();
    assert_eq!(snapshot.depth, 1);
    assert_eq!(snapshot.max_depth, 1);
    assert_eq!(snapshot.wakeups, 1);
    assert!(matches!(
        receiver.try_recv(),
        Ok(EngineCommand::Pause { .. })
    ));
    assert!(matches!(
        receiver.try_recv(),
        Err(EngineCommandTryRecvError::Empty)
    ));

    // Once the first command has been dequeued, a later state edge is
    // accepted again rather than being suppressed forever.
    sender.send(pause()).unwrap();
    assert!(matches!(
        receiver.try_recv(),
        Ok(EngineCommand::Pause { .. })
    ));
}

#[test]
fn concurrent_duplicate_controls_have_one_committed_queue_entry() {
    let (sender, mut receiver) = channel();
    let mut producers = Vec::new();
    for _ in 0..32 {
        let sender = sender.clone();
        producers.push(std::thread::spawn(move || {
            sender
                .send(EngineCommand::Pause {
                    gid: GroupId::new(8),
                })
                .unwrap();
        }));
    }
    for producer in producers {
        producer.join().unwrap();
    }

    assert_eq!(sender.snapshot().depth, 1);
    assert!(matches!(
        receiver.try_recv(),
        Ok(EngineCommand::Pause { .. })
    ));
    assert!(matches!(
        receiver.try_recv(),
        Err(EngineCommandTryRecvError::Empty)
    ));
}

#[test]
fn full_normal_queue_rejects_without_growing_metrics() {
    let (sender, mut receiver) = channel();
    for gid in 1..=ENGINE_COMMAND_CAPACITY as u64 {
        sender.send(add_command(gid)).unwrap();
    }

    let before = sender.snapshot();
    assert_eq!(before.depth, ENGINE_COMMAND_CAPACITY);
    assert_eq!(before.max_depth, ENGINE_COMMAND_CAPACITY);
    assert_eq!(before.wakeups, 1);
    assert_eq!(
        sender.send(add_command(ENGINE_COMMAND_CAPACITY as u64 + 1)),
        Err(EngineCommandSendError::Full)
    );
    assert_eq!(sender.snapshot().depth, ENGINE_COMMAND_CAPACITY);
    assert_eq!(sender.snapshot().max_depth, ENGINE_COMMAND_CAPACITY);
    assert_eq!(sender.snapshot().wakeups, 1);

    // A full normal queue must not block shutdown/control traffic.
    sender
        .send(EngineCommand::ForceHaltAll {
            reason: HaltReason::ShutdownSignal,
        })
        .unwrap();
    assert!(matches!(
        receiver.try_recv(),
        Ok(EngineCommand::ForceHaltAll { .. })
    ));
}

#[test]
fn closed_queue_rejects_and_receiver_reports_closed() {
    let (sender, receiver) = channel();
    drop(receiver);

    assert_eq!(
        sender.send(EngineCommand::PauseAll),
        Err(EngineCommandSendError::Closed)
    );
    assert_eq!(sender.snapshot(), EngineCommandQueueSnapshot::default());

    let (sender, mut receiver) = channel();
    drop(sender);
    assert!(matches!(
        receiver.try_recv(),
        Err(EngineCommandTryRecvError::Closed)
    ));
}
