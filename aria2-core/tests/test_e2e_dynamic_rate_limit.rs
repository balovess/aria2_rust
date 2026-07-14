//! End-to-end test: dynamic rate limit change via RPC `aria2.changeOption`.
//!
//! This test starts a real HTTP download, measures the download speed at an
//! initial rate limit, then calls `aria2.changeOption` through the `RpcEngine`
//! (the same code path the real RPC server uses) to increase the rate and
//! verifies the actual throughput changes accordingly.

mod fixtures;
use aria2_core::engine::command::Command;
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_rpc::engine::RpcEngine;
use aria2_rpc::json_rpc::JsonRpcRequest;
use fixtures::test_server::TestServer;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// Measure download throughput (bytes/sec) over a `window` by polling
/// `RequestGroup::get_completed_length()` (lock-free atomic read).
async fn measure_speed(
    group: &Arc<RwLock<aria2_core::request::request_group::RequestGroup>>,
    window: Duration,
) -> u64 {
    let start_bytes = group.read().await.get_completed_length();
    tokio::time::sleep(window).await;
    let end_bytes = group.read().await.get_completed_length();
    let delta = end_bytes.saturating_sub(start_bytes);
    ((delta as f64) / window.as_secs_f64()) as u64
}

/// Send an `aria2.changeOption` RPC request and assert it succeeds.
async fn rpc_change_option(engine: &RpcEngine, gid_hex: &str, options: serde_json::Value) {
    let req = JsonRpcRequest::new("aria2.changeOption", json!([gid_hex, options])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(
        resp.is_success(),
        "aria2.changeOption should succeed: {:?}",
        resp.error
    );
}

/// Wait until `completed_length` advances past `threshold` or `timeout` elapses.
///
/// This ensures the download has truly started and the initial burst is
/// consumed before we begin measuring — otherwise the burst inflates the
/// first speed sample.
async fn wait_until_progress(
    group: &Arc<RwLock<aria2_core::request::request_group::RequestGroup>>,
    threshold: u64,
    timeout: Duration,
) -> u64 {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let bytes = group.read().await.get_completed_length();
        if bytes >= threshold {
            return bytes;
        }
        if tokio::time::Instant::now() >= deadline {
            return bytes;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn test_e2e_rpc_change_option_increases_download_rate() {
    let server = TestServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("{}/files/large.bin", server.base_url());

    // Initial rate: 80 KB/s — slow enough to be clearly distinguishable.
    let options = DownloadOptions {
        max_download_limit: Some(80_000),
        ..Default::default()
    };

    // Wire RpcEngine to a shared RequestGroupMan — same setup the real
    // application uses in RPC mode.
    let group_man = Arc::new(RwLock::new(RequestGroupMan::new()));
    let engine = RpcEngine::new().with_group_man(group_man.clone());

    // Register the download group with a known GID so we can reference it
    // in the RPC call.
    let gid = GroupId::new(42);
    group_man
        .read()
        .await
        .add_group_with_gid(gid, vec![url.clone()], options.clone())
        .await
        .unwrap();
    let gid_hex = gid.to_hex_string();
    let group = group_man
        .read()
        .await
        .group_by_hex(&gid_hex)
        .expect("group should be registered");

    // Create DownloadCommand with the shared group Arc and spawn it.
    let mut cmd =
        DownloadCommand::new_with_group(group.clone(), &url, &options, dir.path().to_str(), None)
            .expect("Failed to create DownloadCommand");

    let download_task = tokio::spawn(async move {
        let _ = cmd.execute().await;
    });

    // Wait for the download to start and the initial burst (256 KB) to be
    // consumed. We wait until at least 512 KB has been downloaded — past the
    // burst — so the first measurement reflects the throttled rate, not the
    // burst. Timeout at 10 seconds.
    let initial = wait_until_progress(&group, 512 * 1024, Duration::from_secs(10)).await;
    assert!(
        initial >= 512 * 1024,
        "Download should have progressed past 512KB by now, got {} bytes",
        initial
    );

    // Phase 1: measure speed at 80 KB/s.
    // At 80 KB/s, 256 KB (progress-update granularity) takes 3.2 s, so a 5 s
    // window captures at least one full progress update.
    let speed_80k = measure_speed(&group, Duration::from_secs(5)).await;
    println!("Speed at 80 KB/s limit: {} bytes/s", speed_80k);

    // Call aria2.changeOption via RPC to increase the rate to 500 KB/s.
    // Value must be a JSON number (the handler uses as_u64()).
    rpc_change_option(&engine, &gid_hex, json!({"max-download-limit": 500_000})).await;

    // Wait for the new rate to take effect (one refill cycle ~1 s).
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Phase 2: measure speed at 500 KB/s.
    // At 500 KB/s, 256 KB takes 0.5 s, so a 3 s window captures ~6 updates.
    let speed_500k = measure_speed(&group, Duration::from_secs(3)).await;
    println!("Speed at 500 KB/s limit: {} bytes/s", speed_500k);

    // Cleanup.
    download_task.abort();

    // Assertions.
    // The 80 KB/s measurement may include partial burst remnants, so we use
    // a generous upper bound. The key assertion is that the 500 KB/s phase
    // is significantly faster.
    assert!(
        speed_80k < 200_000,
        "Speed at 80KB/s should be < 200KB/s, got {} bytes/s",
        speed_80k
    );
    assert!(
        speed_500k > speed_80k * 2,
        "Speed should at least double after rate increase: {} vs {} bytes/s",
        speed_500k,
        speed_80k
    );
    assert!(
        speed_500k > 200_000,
        "Speed at 500KB/s should be > 200KB/s, got {} bytes/s",
        speed_500k
    );
}

#[tokio::test]
async fn test_e2e_rpc_change_option_to_unlimited() {
    let server = TestServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("{}/files/large.bin", server.base_url());

    // Initial rate: 100 KB/s — slow enough to throttle, fast enough for
    // progress updates within a reasonable window.
    let options = DownloadOptions {
        max_download_limit: Some(100_000),
        ..Default::default()
    };

    let group_man = Arc::new(RwLock::new(RequestGroupMan::new()));
    let engine = RpcEngine::new().with_group_man(group_man.clone());

    let gid = GroupId::new(7);
    group_man
        .read()
        .await
        .add_group_with_gid(gid, vec![url.clone()], options.clone())
        .await
        .unwrap();
    let gid_hex = gid.to_hex_string();
    let group = group_man
        .read()
        .await
        .group_by_hex(&gid_hex)
        .expect("group should be registered");

    let mut cmd =
        DownloadCommand::new_with_group(group.clone(), &url, &options, dir.path().to_str(), None)
            .expect("Failed to create DownloadCommand");

    let download_task = tokio::spawn(async move {
        let _ = cmd.execute().await;
    });

    // Wait for the download to start and burst to be consumed.
    let initial = wait_until_progress(&group, 512 * 1024, Duration::from_secs(10)).await;
    assert!(
        initial >= 512 * 1024,
        "Download should have progressed past 512KB by now, got {} bytes",
        initial
    );

    // Phase 1: measure speed at 100 KB/s.
    let speed_limited = measure_speed(&group, Duration::from_secs(4)).await;
    println!("Speed at 100 KB/s limit: {} bytes/s", speed_limited);

    // Record bytes just before the rate change.
    let bytes_before = group.read().await.get_completed_length();

    // Call aria2.changeOption via RPC to set unlimited (0 = unlimited).
    rpc_change_option(&engine, &gid_hex, json!({"max-download-limit": 0})).await;

    // On localhost, once the rate is lifted the remaining 9 MB may download
    // in well under a second. So we measure over a short 2 s window starting
    // immediately after the RPC call (no intermediate sleep) to capture the
    // speed burst. If the download already completed during this window, the
    // delta will still be large (proving the unlimited rate works).
    let speed_unlimited = measure_speed(&group, Duration::from_secs(2)).await;
    let bytes_after = group.read().await.get_completed_length();
    println!(
        "Speed unlimited: {} bytes/s (bytes {} -> {})",
        speed_unlimited, bytes_before, bytes_after
    );

    download_task.abort();

    assert!(
        speed_limited < 250_000,
        "Speed at 100KB/s should be < 250KB/s, got {} bytes/s",
        speed_limited
    );
    assert!(
        speed_unlimited > speed_limited * 2,
        "Unlimited speed should be significantly faster: {} vs {} bytes/s (bytes {} -> {})",
        speed_unlimited,
        speed_limited,
        bytes_before,
        bytes_after
    );
}
