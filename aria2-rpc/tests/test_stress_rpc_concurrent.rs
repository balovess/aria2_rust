//! Concurrency tests for the transport/backend seam.

mod common;

use aria2_rpc::json_rpc::JsonRpcRequest;
use aria2_rpc::server::RpcAuthMiddleware;
use aria2_rpc::websocket::{DownloadEvent, EventPublisher, NotificationBatcher};
use common::test_engine;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, Semaphore};

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_read_and_add_requests_return_without_loss() {
    let engine = Arc::new(test_engine());
    let semaphore = Arc::new(Semaphore::new(64));
    let mut handles = Vec::new();

    for index in 0..300 {
        let engine = Arc::clone(&engine);
        let semaphore = Arc::clone(&semaphore);
        handles.push(tokio::spawn(async move {
            let _permit = semaphore.acquire_owned().await.unwrap();
            let (method, params) = if index % 3 == 0 {
                (
                    "aria2.addUri",
                    json!([[format!("http://example.test/{index}")]]),
                )
            } else {
                ("aria2.getGlobalStat", json!([]))
            };
            engine
                .handle_request(&JsonRpcRequest::new(method, params).with_id(index))
                .await
        }));
    }

    let responses = futures::future::join_all(handles).await;
    assert_eq!(responses.len(), 300);
    assert!(responses.iter().all(|response| {
        response
            .as_ref()
            .map(|response| response.is_success())
            .unwrap_or(false)
    }));
    assert_eq!(engine.task_count().await, 100);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn event_publisher_handles_bursts_without_blocking() {
    let publisher = Arc::new(EventPublisher::new(1024));
    let mut subscribers = Vec::new();
    for index in 0..4 {
        subscribers.push((
            index,
            publisher.subscribe(format!("client-{index}"), None).await,
        ));
    }
    let counts = Arc::new(RwLock::new(vec![0usize; 4]));
    let mut receivers = Vec::new();
    for (index, mut receiver) in subscribers {
        let counts = Arc::clone(&counts);
        receivers.push(tokio::spawn(async move {
            for _ in 0..100 {
                if receiver.recv().await.is_ok() {
                    counts.write().await[index] += 1;
                }
            }
        }));
    }

    for index in 0..100 {
        publisher
            .publish_event(DownloadEvent::download_start(format!("gid-{index}")))
            .unwrap();
    }
    futures::future::join_all(receivers).await;
    assert!(counts.read().await.iter().all(|count| *count == 100));
}

#[test]
fn notification_batcher_deduplicates_repeated_gids() {
    let mut batcher = NotificationBatcher::new()
        .with_max_batch_size(100)
        .with_flush_interval_ms(1);
    for index in 0..100 {
        batcher.push(DownloadEvent::download_complete(format!(
            "gid-{}",
            index % 10
        )));
    }
    std::thread::sleep(Duration::from_millis(5));
    let batch = batcher.maybe_flush().expect("batch should flush");
    let (_, deduped) = batcher.stats();
    assert_eq!(batch.len(), 10);
    assert!(deduped >= 90);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authentication_isolated_per_request_under_concurrency() {
    let engine = Arc::new(test_engine().with_auth_middleware(RpcAuthMiddleware::new("secret")));
    let mut handles = Vec::new();
    for index in 0..100 {
        let engine = Arc::clone(&engine);
        handles.push(tokio::spawn(async move {
            let token = if index % 2 == 0 {
                "token:secret"
            } else {
                "token:wrong"
            };
            engine
                .handle_request(
                    &JsonRpcRequest::new("aria2.getVersion", json!([token])).with_id(index),
                )
                .await
        }));
    }
    let responses = futures::future::join_all(handles).await;
    for (index, response) in responses.into_iter().enumerate() {
        assert_eq!(response.unwrap().is_success(), index % 2 == 0);
    }
}

#[test]
fn high_volume_batching_stays_bounded() {
    let mut batcher = NotificationBatcher::new()
        .with_max_batch_size(32)
        .with_flush_interval_ms(100);
    let start = Instant::now();
    for index in 0..1000 {
        batcher.push(DownloadEvent::download_error(format!("gid-{index}")));
    }
    assert!(start.elapsed() < Duration::from_secs(1));
    let (sent, _) = batcher.stats();
    assert!(sent > 0);
}
