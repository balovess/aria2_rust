use super::*;

#[test]
fn test_determine_stop_event_complete() {
    assert_eq!(
        determine_stop_event(true, false, false),
        Some(DownloadEvent::Complete)
    );
}

#[test]
fn test_determine_stop_event_error() {
    assert_eq!(
        determine_stop_event(false, true, false),
        Some(DownloadEvent::Error)
    );
}

#[test]
fn test_determine_stop_event_paused() {
    assert_eq!(
        determine_stop_event(false, false, true),
        Some(DownloadEvent::Pause)
    );
}

#[test]
fn test_determine_stop_event_generic_stop() {
    assert_eq!(
        determine_stop_event(false, false, false),
        Some(DownloadEvent::Stop)
    );
}

#[test]
fn test_event_names() {
    assert_eq!(DownloadEvent::Start.name(), "on-download-start");
    assert_eq!(DownloadEvent::Complete.name(), "on-download-complete");
    assert_eq!(DownloadEvent::Error.name(), "on-download-error");
    assert_eq!(DownloadEvent::Pause.name(), "on-download-pause");
    assert_eq!(DownloadEvent::Stop.name(), "on-download-stop");
    assert_eq!(DownloadEvent::BtComplete.name(), "on-bt-download-complete");
}

#[test]
fn test_global_hook_registration() {
    let hooks = DownloadEventHooks::new();
    hooks.set_global_hook(DownloadEvent::Start, "/usr/bin/echo".to_string());
    hooks.set_global_hook(DownloadEvent::Complete, "/usr/bin/notify".to_string());

    let guard = hooks.global_hooks.recover();
    assert_eq!(guard.len(), 2);
    assert!(guard.iter().any(|(e, _)| *e == DownloadEvent::Start));
    assert!(guard.iter().any(|(e, _)| *e == DownloadEvent::Complete));
}

#[test]
fn test_global_hook_replacement() {
    let hooks = DownloadEventHooks::new();
    hooks.set_global_hook(DownloadEvent::Start, "/usr/bin/old".to_string());
    hooks.set_global_hook(DownloadEvent::Start, "/usr/bin/new".to_string());

    let guard = hooks.global_hooks.recover();
    let start_hook = guard.iter().find(|(e, _)| *e == DownloadEvent::Start);
    assert_eq!(start_hook.unwrap().1, "/usr/bin/new");
}

#[test]
fn test_global_hook_removal() {
    let hooks = DownloadEventHooks::new();
    hooks.set_global_hook(DownloadEvent::Start, "/usr/bin/echo".to_string());
    hooks.remove_global_hook(DownloadEvent::Start);

    let guard = hooks.global_hooks.recover();
    assert!(guard.is_empty());
}

// ── Observer (DownloadEventListener) tests ───────────────────────────

/// Records every event it is notified about.
#[derive(Default)]
struct RecordingListener {
    seen: std::sync::Mutex<Vec<(DownloadEvent, String)>>,
    metadata_seen: std::sync::Mutex<Vec<MetadataResolvedEvent>>,
}

impl RecordingListener {
    fn events(&self) -> Vec<(DownloadEvent, String)> {
        self.seen.lock().unwrap().clone()
    }

    fn metadata_events(&self) -> Vec<MetadataResolvedEvent> {
        self.metadata_seen.lock().unwrap().clone()
    }
}

impl DownloadEventListener for RecordingListener {
    fn on_download_event(&self, event: DownloadEvent, gid: &str) {
        self.seen.lock().unwrap().push((event, gid.to_string()));
    }

    fn on_metadata_resolved(&self, event: &MetadataResolvedEvent) {
        self.metadata_seen.lock().unwrap().push(event.clone());
    }
}

#[test]
fn test_add_listener_tracks_registration() {
    let hooks = DownloadEventHooks::new();
    assert_eq!(hooks.listener_count(), 0);
    hooks.add_listener(Arc::new(RecordingListener::default()));
    hooks.add_listener(Arc::new(RecordingListener::default()));
    assert_eq!(hooks.listener_count(), 2);
}

#[test]
fn test_listener_can_be_removed_explicitly() {
    let hooks = DownloadEventHooks::new();
    let id = hooks.add_listener_with_id(Arc::new(RecordingListener::default()));
    assert_eq!(hooks.listener_count(), 1);
    assert!(hooks.remove_listener(id));
    assert_eq!(hooks.listener_count(), 0);
    assert!(!hooks.remove_listener(id));
}

struct ToggleListener {
    alive: std::sync::atomic::AtomicBool,
}

impl DownloadEventListener for ToggleListener {
    fn on_download_event(&self, _event: DownloadEvent, _gid: &str) {}

    fn is_alive(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[test]
fn test_registration_prunes_dead_listeners() {
    let hooks = DownloadEventHooks::new();
    let stale = Arc::new(ToggleListener {
        alive: std::sync::atomic::AtomicBool::new(true),
    });
    hooks.add_listener(stale.clone());
    stale
        .alive
        .store(false, std::sync::atomic::Ordering::Relaxed);

    hooks.add_listener(Arc::new(RecordingListener::default()));

    assert_eq!(hooks.listener_count(), 1);
}

#[test]
fn test_notify_listeners_delivers_to_all_listeners() {
    let hooks = DownloadEventHooks::new();
    let a = Arc::new(RecordingListener::default());
    let b = Arc::new(RecordingListener::default());
    hooks.add_listener(a.clone());
    hooks.add_listener(b.clone());

    hooks.notify_listeners(DownloadEvent::Complete, "00000000000000ff");

    let expected = vec![(DownloadEvent::Complete, "00000000000000ff".to_string())];
    assert_eq!(a.events(), expected);
    assert_eq!(b.events(), expected);
}

#[test]
fn test_notify_metadata_resolved_delivers_structured_event() {
    let hooks = DownloadEventHooks::new();
    let listener = Arc::new(RecordingListener::default());
    hooks.add_listener(listener.clone());

    hooks.notify_metadata_resolved(MetadataResolvedEvent::new(
        crate::request::request_group::GroupId::new(10),
        vec![crate::request::request_group::GroupId::new(11)],
    ));

    assert_eq!(
        listener.metadata_events(),
        vec![MetadataResolvedEvent::new(
            crate::request::request_group::GroupId::new(10),
            vec![crate::request::request_group::GroupId::new(11)],
        )]
    );
}

#[tokio::test]
async fn test_event_stream_receives_lifecycle_and_metadata_events() {
    let hooks = DownloadEventHooks::new();
    let mut stream = hooks.subscribe();

    hooks.notify_listeners(DownloadEvent::Start, "0000000000000001");
    assert_eq!(
        stream.recv().await.expect("lifecycle event"),
        DownloadNotification::Lifecycle {
            event: DownloadEvent::Start,
            gid: "0000000000000001".to_string(),
        }
    );

    let metadata = MetadataResolvedEvent::new(
        crate::request::request_group::GroupId::new(10),
        vec![crate::request::request_group::GroupId::new(11)],
    );
    hooks.notify_metadata_resolved(metadata.clone());
    assert_eq!(
        stream.recv().await.expect("metadata event"),
        DownloadNotification::MetadataResolved(metadata)
    );
}

#[tokio::test]
async fn metadata_stream_helper_filters_by_metadata_or_download_gid() {
    let hooks = DownloadEventHooks::new();
    let mut stream = hooks.subscribe();
    let requested_gid = crate::request::request_group::GroupId::new(11);
    let unrelated = MetadataResolvedEvent::new(
        crate::request::request_group::GroupId::new(20),
        vec![crate::request::request_group::GroupId::new(21)],
    );
    let matching = MetadataResolvedEvent::new(
        crate::request::request_group::GroupId::new(10),
        vec![requested_gid],
    );

    hooks.notify_metadata_resolved(unrelated);
    hooks.notify_metadata_resolved(matching.clone());

    assert_eq!(
        stream
            .recv_metadata_for(requested_gid)
            .await
            .expect("matching metadata event"),
        matching
    );
}

#[tokio::test]
async fn terminal_stream_helper_filters_by_gid_and_event_kind() {
    let hooks = DownloadEventHooks::new();
    let mut stream = hooks.subscribe();
    let requested_gid = crate::request::request_group::GroupId::new(11);

    hooks.notify_listeners(DownloadEvent::Start, "000000000000000b");
    hooks.notify_listeners(DownloadEvent::Complete, "000000000000000a");
    hooks.notify_listeners(DownloadEvent::Stop, "000000000000000b");

    assert_eq!(
        stream
            .recv_terminal_for(requested_gid)
            .await
            .expect("matching terminal event"),
        DownloadEvent::Stop
    );
}

#[tokio::test]
async fn terminal_stream_helper_waits_past_bt_payload_completion() {
    let hooks = DownloadEventHooks::new();
    let mut stream = hooks.subscribe();
    let requested_gid = crate::request::request_group::GroupId::new(12);
    let gid = requested_gid.to_hex_string();

    hooks.notify_listeners(DownloadEvent::BtComplete, &gid);
    hooks.notify_listeners(DownloadEvent::Complete, &gid);

    assert_eq!(
        stream
            .recv_terminal_for(requested_gid)
            .await
            .expect("seeding completion is terminal"),
        DownloadEvent::Complete
    );
}

/// Regression test for the P0 defect: observers were only reached when a
/// `--on-download-*` shell command happened to be configured, so RPC
/// clients never saw `aria2.onDownloadComplete` in a default deployment.
#[test]
fn test_fire_event_with_params_notifies_listener_without_shell_command() {
    let hooks = DownloadEventHooks::new();
    let listener = Arc::new(RecordingListener::default());
    hooks.add_listener(listener.clone());

    // Empty command == no `--on-download-complete` configured.
    hooks.fire_event_with_params(
        DownloadEvent::Complete,
        "0000000000000001",
        1,
        "/tmp/a.bin",
        "",
    );

    assert_eq!(
        listener.events(),
        vec![(DownloadEvent::Complete, "0000000000000001".to_string())],
        "observers must be notified even with no shell hook configured"
    );
}

#[test]
fn test_one_shot_events_are_deduplicated_per_gid() {
    let hooks = DownloadEventHooks::new();
    let listener = Arc::new(RecordingListener::default());
    hooks.add_listener(listener.clone());

    // Same terminal event emitted twice for the same download (state
    // transition + engine-loop demotion) must reach the observer once.
    hooks.notify_listeners(DownloadEvent::Complete, "000000000000000a");
    hooks.notify_listeners(DownloadEvent::Complete, "000000000000000a");
    // A different GID is a different download.
    hooks.notify_listeners(DownloadEvent::Complete, "000000000000000b");
    // A different terminal event for the first download still passes.
    hooks.notify_listeners(DownloadEvent::BtComplete, "000000000000000a");

    assert_eq!(
        listener.events(),
        vec![
            (DownloadEvent::Complete, "000000000000000a".to_string()),
            (DownloadEvent::Complete, "000000000000000b".to_string()),
            (DownloadEvent::BtComplete, "000000000000000a".to_string()),
        ]
    );
}

#[test]
fn test_repeatable_events_are_not_deduplicated() {
    let hooks = DownloadEventHooks::new();
    let listener = Arc::new(RecordingListener::default());
    hooks.add_listener(listener.clone());

    // Pause/unpause cycles legitimately repeat Start and Pause.
    hooks.notify_listeners(DownloadEvent::Pause, "000000000000000c");
    hooks.notify_listeners(DownloadEvent::Start, "000000000000000c");
    hooks.notify_listeners(DownloadEvent::Pause, "000000000000000c");

    assert_eq!(listener.events().len(), 3);
}

#[test]
fn test_notify_listeners_is_noop_without_listeners() {
    let hooks = DownloadEventHooks::new();
    // Must not panic and must not consume de-duplication budget, so a
    // listener registered later still receives the event.
    hooks.notify_listeners(DownloadEvent::Complete, "000000000000000d");

    let listener = Arc::new(RecordingListener::default());
    hooks.add_listener(listener.clone());
    hooks.notify_listeners(DownloadEvent::Complete, "000000000000000d");

    assert_eq!(listener.events().len(), 1);
}

#[test]
fn test_shared_bus_is_a_singleton() {
    let a = DownloadEventHooks::shared();
    let b = DownloadEventHooks::shared();
    assert!(Arc::ptr_eq(a, b), "shared() must return the same instance");
}

#[test]
fn test_is_once_per_download_classification() {
    assert!(DownloadEvent::Complete.is_once_per_download());
    assert!(DownloadEvent::Error.is_once_per_download());
    assert!(DownloadEvent::BtComplete.is_once_per_download());
    assert!(!DownloadEvent::Start.is_once_per_download());
    assert!(!DownloadEvent::Pause.is_once_per_download());
    assert!(!DownloadEvent::Stop.is_once_per_download());
}

#[test]
fn test_terminal_event_classification() {
    assert!(DownloadEvent::Stop.is_terminal());
    assert!(DownloadEvent::Complete.is_terminal());
    assert!(DownloadEvent::Error.is_terminal());
    assert!(!DownloadEvent::BtComplete.is_terminal());
    assert!(!DownloadEvent::Start.is_terminal());
    assert!(!DownloadEvent::Pause.is_terminal());
}

#[test]
fn test_one_shot_ledger_rotates_and_stays_bounded() {
    let mut ledger = OneShotLedger::default();
    for i in 0..(DEDUP_GENERATION_CAPACITY + 10) {
        assert!(ledger.claim((None, format!("{:016x}", i), DownloadEvent::Complete)));
    }
    // Rotation happened, so neither generation exceeds the cap.
    assert!(ledger.current.len() <= DEDUP_GENERATION_CAPACITY);
    assert!(ledger.previous.len() <= DEDUP_GENERATION_CAPACITY);
    // The most recent entries are still remembered.
    assert!(!ledger.claim((
        None,
        format!("{:016x}", DEDUP_GENERATION_CAPACITY + 9),
        DownloadEvent::Complete
    )));
}
