use super::*;

#[test]
fn authority_key_includes_scheme_and_ignores_path() {
    assert_eq!(
        authority_key("HTTP://Example.TEST/download/file").as_deref(),
        Some("http://example.test:80")
    );
    assert_eq!(
        authority_key("https://[::1]/file").as_deref(),
        Some("https://[::1]:443")
    );
    assert_ne!(
        authority_key("http://example.test/file"),
        authority_key("https://example.test/file")
    );
}

#[test]
fn leases_keep_completed_requests_in_flight_until_consumed() {
    let state = Arc::new(ExecutorState::new(
        &["http://example.test:80".into()],
        4,
        4,
        1,
    ));
    let authority = state.authority("http://example.test:80").unwrap();
    let (lease, client_index) = state.try_acquire(&authority, 2).unwrap();
    assert_eq!(client_index, 0);
    assert_eq!(state.total_in_flight.load(Ordering::Acquire), 1);
    assert_eq!(authority.in_flight.load(Ordering::Acquire), 1);
    assert_eq!(authority.client_in_flight[0].load(Ordering::Acquire), 1);
    drop(lease);
    assert_eq!(state.total_in_flight.load(Ordering::Acquire), 0);
    assert_eq!(authority.in_flight.load(Ordering::Acquire), 0);
    assert_eq!(authority.client_in_flight[0].load(Ordering::Acquire), 0);
}

#[test]
fn total_and_authority_limits_are_independent() {
    let state = Arc::new(ExecutorState::new(
        &["http://one.test:80".into(), "http://two.test:80".into()],
        2,
        2,
        1,
    ));
    let one = state.authority("http://one.test:80").unwrap();
    let two = state.authority("http://two.test:80").unwrap();
    let (first, _) = state.try_acquire(&one, 2).unwrap();
    let (second, _) = state.try_acquire(&one, 2).unwrap();
    assert!(state.try_acquire(&two, 2).is_none());
    drop(first);
    drop(second);
    assert!(state.try_acquire(&two, 2).is_some());
}

#[test]
fn h2_prefers_streams_on_each_session_before_opening_another() {
    let state = Arc::new(ExecutorState::new(
        &["https://h2.test:443".into()],
        16,
        16,
        4,
    ));
    let authority = state.authority("https://h2.test:443").unwrap();
    authority.protocol.store(2, Ordering::Release);
    authority.active_h2_sessions.store(1, Ordering::Release);
    let stream_probe: Vec<_> = (0..4)
        .map(|_| state.try_acquire(&authority, 16).unwrap())
        .collect();
    assert!(stream_probe.iter().all(|(_, index)| *index == 0));
    drop(stream_probe);

    authority.active_h2_sessions.store(2, Ordering::Release);
    authority.target.store(4, Ordering::Release);
    let session_probe: Vec<_> = (0..4)
        .map(|_| state.try_acquire(&authority, 16).unwrap())
        .collect();
    let slots: Vec<_> = session_probe.iter().map(|(_, index)| *index).collect();
    assert_eq!(slots, vec![0, 1, 0, 1]);
}

#[test]
fn h1_spreads_active_requests_across_session_pools() {
    let state = Arc::new(ExecutorState::new(&["http://h1.test:80".into()], 16, 16, 4));
    let authority = state.authority("http://h1.test:80").unwrap();
    authority.protocol.store(1, Ordering::Release);
    let reservations: Vec<_> = (0..4)
        .map(|_| state.try_acquire(&authority, 16).unwrap())
        .collect();
    let slots: Vec<_> = reservations.iter().map(|(_, index)| *index).collect();
    assert_eq!(slots, vec![0, 1, 2, 3]);
}

#[test]
fn split_budget_is_distinct_from_connection_ceiling_for_http2() {
    let state = Arc::new(ExecutorState::new(
        &["https://single-session.test:443".into()],
        16,
        1,
        1,
    ));
    let authority = state.authority("https://single-session.test:443").unwrap();
    authority.protocol.store(2, Ordering::Release);
    authority.active_h2_sessions.store(1, Ordering::Release);

    let reservations: Vec<_> = (0..16)
        .map(|_| state.try_acquire(&authority, 16).unwrap())
        .collect();
    assert!(reservations.iter().all(|(_, index)| *index == 0));
    assert_eq!(authority.in_flight.load(Ordering::Acquire), 16);
    assert!(state.try_acquire(&authority, 16).is_none());
}

#[test]
fn h2_stream_capacity_grows_on_the_active_session_before_another_is_added() {
    let state = Arc::new(ExecutorState::new(
        &["https://adaptive.test:443".into()],
        16,
        16,
        4,
    ));
    let authority = state.authority("https://adaptive.test:443").unwrap();
    authority.protocol.store(2, Ordering::Release);
    authority.active_h2_sessions.store(1, Ordering::Release);
    authority.target.store(8, Ordering::Release);

    let reservations: Vec<_> = (0..8)
        .map(|_| state.try_acquire(&authority, 16).unwrap())
        .collect();

    assert!(reservations.iter().all(|(_, index)| *index == 0));
    assert_eq!(authority.in_flight.load(Ordering::Acquire), 8);
}

#[tokio::test]
async fn completion_event_reclaims_only_its_task() {
    crate::http::client_pool::ensure_rustls_provider();
    let (result_tx, result_rx) = mpsc::channel(1);
    let mut executor = HttpSegmentRequestExecutor {
        result_rx,
        result_tx,
        clients: vec![reqwest::Client::new()],
        request_policy: HttpRequestPolicy::default(),
        cookie_helper: CookieHelper::new(Arc::new(crate::http::cookie::CookieStorage::new()), None),
        auth_options: AuthResolveOptions::default(),
        netrc_path: None,
        state: Arc::new(ExecutorState::new(&[], 1, 1, 1)),
        total_limit: 1,
        tasks: vec![
            RunningTask {
                id: 1,
                segment_index: 1,
                handle: tokio::spawn(async {}),
            },
            RunningTask {
                id: 2,
                segment_index: 2,
                handle: tokio::spawn(async {}),
            },
        ],
        next_task_id: 3,
    };

    executor.reap_task(2).await;
    assert_eq!(executor.tasks.len(), 1);
    assert_eq!(executor.tasks[0].id, 1);

    executor.cancel().await;
}

#[tokio::test]
async fn selecting_next_result_does_not_drop_a_received_completion() {
    crate::http::client_pool::ensure_rustls_provider();
    let authority_key = "https://cancel-safe.test:443".to_owned();
    let state = Arc::new(ExecutorState::new(
        std::slice::from_ref(&authority_key),
        1,
        1,
        1,
    ));
    let authority = state.authority(&authority_key).unwrap();
    let (lease, _) = state.try_acquire(&authority, 1).unwrap();
    let (result_tx, result_rx) = mpsc::channel(1);
    let executor_result_tx = result_tx.clone();
    let (sent_tx, sent_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        result_tx
            .send(HttpSegmentRequestResult {
                task_id: 1,
                segment_index: 0,
                authority_key,
                result: Ok(1),
                range_size_limit: 1,
                range_size_rejected: false,
                peer_addr: None,
                _lease: lease,
            })
            .await
            .unwrap();
        let _ = sent_tx.send(());
        let _ = release_rx.await;
    });
    let mut executor = HttpSegmentRequestExecutor {
        result_rx,
        result_tx: executor_result_tx,
        clients: vec![reqwest::Client::new()],
        request_policy: HttpRequestPolicy::default(),
        cookie_helper: CookieHelper::new(Arc::new(crate::http::cookie::CookieStorage::new()), None),
        auth_options: AuthResolveOptions::default(),
        netrc_path: None,
        state: Arc::clone(&state),
        total_limit: 1,
        tasks: vec![RunningTask {
            id: 1,
            segment_index: 0,
            handle: task,
        }],
        next_task_id: 2,
    };

    let result = tokio::select! {
        biased;
        result = executor.next_result() => Some(result.unwrap()),
        _ = sent_rx => None,
    };
    assert!(
        result.is_some(),
        "the result must win selection once it has been received"
    );

    let result = result.unwrap();
    let _ = release_tx.send(());
    executor.reap_task(result.task_id).await;
    assert_eq!(state.total_in_flight.load(Ordering::Acquire), 1);
    drop(result);
    assert_eq!(state.total_in_flight.load(Ordering::Acquire), 0);
}
