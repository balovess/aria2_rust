use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use aria2_core::rate_limiter::{RateLimiter, RateLimiterConfig, TokenBucket, ThrottledWriter};
use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::request::request_group::{RequestGroup, GroupId, DownloadOptions};
use aria2_core::filesystem::disk_writer::{DiskWriter, ByteArrayDiskWriter};

fn create_test_group(uri: &str) -> Arc<RwLock<RequestGroup>> {
    let options = DownloadOptions {
        split: None,
        max_connection_per_server: None,
        max_download_limit: Some(50000),
        max_upload_limit: None,
        dir: None,
        out: None,
        seed_time: None,
        seed_ratio: None,
        checksum: None,
        cookie_file: None,
        cookies: None,
        bt_force_encrypt: false,
        bt_require_crypto: false,
        enable_dht: false,
        dht_listen_port: None,
        enable_public_trackers: false,
        bt_piece_selection_strategy: "rarest-first".to_string(),
        bt_endgame_threshold: 20,
        max_retries: 3,
        retry_wait: 1,
        http_proxy: None,
        dht_file_path: None,
    };
    let group = RequestGroup::new(GroupId::new(1), vec![uri.to_string()], options);
    Arc::new(RwLock::new(group))
}

#[tokio::test]
async fn test_rate_limiter_token_bucket_refill() {
    let mut tb = TokenBucket::new(10000, Some(2000));
    tb.acquire(1500).await;
    assert!(tb.available_tokens() < 1000.0, "should have consumed tokens");
}

#[tokio::test]
async fn test_rate_limiter_config_is_limited() {
    let cfg = RateLimiterConfig::new(Some(1024), None);
    assert!(cfg.is_limited());
    assert_eq!(cfg.download_rate(), Some(1024));
    assert!(cfg.upload_rate().is_none());

    let cfg_unlimited = RateLimiterConfig::default();
    assert!(!cfg_unlimited.is_limited());
}

#[tokio::test]
async fn test_rate_limiter_clone_shares_state() {
    let cfg = RateLimiterConfig::new(Some(10000), None).with_burst(Some(5000), None);
    let rl1 = RateLimiter::new(&cfg);
    let rl2 = rl1.clone();

    assert!(rl1.is_download_limited());
    assert!(rl2.is_download_limited());

    rl1.acquire_download(3000).await;
    rl2.acquire_download(3000).await;

    let config = rl1.config().await;
    assert!(config.download_rate().is_some());
}

#[tokio::test]
async fn test_throttled_writer_data_integrity() {
    let raw = ByteArrayDiskWriter::new();
    let cfg = RateLimiterConfig::new(Some(1_000_000), None)
        .with_burst(Some(512), None);
    let limiter = RateLimiter::new(&cfg);
    let mut tw = ThrottledWriter::new(raw, limiter);

    let data = b"Hello, Rate-Limited World! This is a test of data integrity.";
    tw.write(data).await.unwrap();
    let result = tw.finalize().await.unwrap();
    assert_eq!(result, data, "throttled writer must preserve data exactly");
}

#[tokio::test]
async fn test_throttled_writer_multiple_writes() {
    let raw = ByteArrayDiskWriter::new();
    let cfg = RateLimiterConfig::new(Some(50_000), None)
        .with_burst(Some(256), None);
    let limiter = RateLimiter::new(&cfg);
    let mut tw = ThrottledWriter::new(raw, limiter);

    for i in 0..10u8 {
        let chunk = vec![i; 100];
        tw.write(&chunk).await.unwrap();
    }

    let result = tw.finalize().await.unwrap();
    assert_eq!(result.len(), 1000);
    for (i, &byte) in result.iter().enumerate() {
        let expected = (i / 100) as u8;
        assert_eq!(byte, expected, "byte at index {} should be {}", i, expected);
    }
}

#[tokio::test]
async fn test_engine_global_lifecycle() {
    use aria2_core::retry::RetryPolicy;

    let policy = RetryPolicy::default();
    let mut engine = DownloadEngine::with_retry_policy(10, policy.clone());

    assert!(engine.global_rate_limiter().is_none(), "no limiter by default");

    engine.set_global_rate_limiter(RateLimiterConfig::new(Some(9999), Some(5555)));
    assert!(engine.global_rate_limiter().is_some());

    let taken = engine.take_global_rate_limiter();
    assert!(taken.is_some());
    assert!(engine.global_rate_limiter().is_none(), "limiter removed after take");
}

#[tokio::test]
async fn test_engine_global_limiter_limits() {
    use aria2_core::retry::RetryPolicy;

    let mut engine = DownloadEngine::with_retry_policy(10, RetryPolicy::default());
    engine.set_global_rate_limiter(
        RateLimiterConfig::new(Some(10000), None).with_burst(Some(1000), None)
    );

    let limiter = engine.global_rate_limiter().unwrap();
    assert!(limiter.is_download_limited());

    let start = Instant::now();
    limiter.acquire_download(2000).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(80),
        "2KB at 10KB/s with 1KB burst should take >= 80ms, got {:?}",
        elapsed
    );
}

#[tokio::test]
async fn test_zero_limit_means_no_limit() {
    let cfg = RateLimiterConfig::new(Some(0), Some(0));
    let rl = RateLimiter::new(&cfg);
    assert!(!rl.is_download_limited(), "0 download limit = unlimited");
    assert!(!rl.is_upload_limited(), "0 upload limit = unlimited");

    let raw = ByteArrayDiskWriter::new();
    let mut tw = ThrottledWriter::new(raw, rl);
    tw.write(&[0xAB; 10000]).await.unwrap();
    let result = tw.finalize().await.unwrap();
    assert_eq!(result.len(), 10000);
}

#[tokio::test]
async fn test_rate_limiter_high_rate_low_latency() {
    let cfg = RateLimiterConfig::new(Some(100_000_000), None)
        .with_burst(Some(1_000_000), None);
    let rl = RateLimiter::new(&cfg);

    let start = Instant::now();
    rl.acquire_download(100_000).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(50),
        "100MB/s rate with 1MB burst should be near-instant for 100KB: got {:?}",
        elapsed
    );
}
