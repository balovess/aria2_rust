use aria2_core::error::{Aria2Error, FatalError, RecoverableError};
use aria2_core::retry::{RetryExecutor, RetryPolicy, RetryStats};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn test_retry_policy_default() {
    let policy = RetryPolicy::default();
    assert_eq!(policy.max_tries(), 5);
}

#[test]
fn test_retry_policy_custom() {
    let policy = RetryPolicy::new(10, Duration::from_secs(2));
    assert_eq!(policy.max_tries(), 10);
    assert_eq!(policy.wait_duration(0), Duration::from_secs(2));
}

#[test]
fn test_should_retry_recoverable_error() {
    let policy = RetryPolicy::new(3, Duration::from_secs(1));

    assert!(policy.should_retry(0, &Aria2Error::Recoverable(RecoverableError::Timeout)));
    assert!(policy.should_retry(1, &Aria2Error::Recoverable(RecoverableError::Timeout)));
    assert!(!policy.should_retry(2, &Aria2Error::Recoverable(RecoverableError::Timeout)));

    assert!(policy.should_retry(
        0,
        &Aria2Error::Recoverable(RecoverableError::ServerError { code: 500 })
    ));
    assert!(policy.should_retry(
        0,
        &Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: "conn reset".into()
        })
    ));
}

#[test]
fn test_should_not_retry_fatal_error() {
    let policy = RetryPolicy::new(3, Duration::from_secs(1));

    assert!(!policy.should_retry(0, &Aria2Error::Fatal(FatalError::DiskSpaceExhausted)));
    assert!(!policy.should_retry(0, &Aria2Error::Fatal(FatalError::Config("bad".into()))));
    assert!(!policy.should_retry(0, &Aria2Error::Network("err".into())));
}

#[test]
fn test_should_not_retry_max_tries_exceeded() {
    let policy = RetryPolicy::new(2, Duration::from_secs(1));

    assert!(!policy.should_retry(2, &Aria2Error::Recoverable(RecoverableError::Timeout)));
    assert!(!policy.should_retry(100, &Aria2Error::Recoverable(RecoverableError::Timeout)));
}

#[test]
fn test_wait_duration_exponential_backoff() {
    let policy =
        RetryPolicy::new(10, Duration::from_secs(1)).with_max_wait(Duration::from_secs(1000));

    assert_eq!(policy.wait_duration(0), Duration::from_secs(1));
    assert_eq!(policy.wait_duration(1), Duration::from_secs(2));
    assert_eq!(policy.wait_duration(2), Duration::from_secs(4));
    assert_eq!(policy.wait_duration(3), Duration::from_secs(8));
    assert_eq!(policy.wait_duration(4), Duration::from_secs(16));
    assert_eq!(policy.wait_duration(9), Duration::from_secs(512));
}

#[test]
fn test_wait_duration_capped_at_max() {
    let policy =
        RetryPolicy::new(10, Duration::from_secs(1)).with_max_wait(Duration::from_secs(10));

    assert_eq!(policy.wait_duration(0), Duration::from_secs(1));
    assert_eq!(policy.wait_duration(1), Duration::from_secs(2));
    assert_eq!(policy.wait_duration(2), Duration::from_secs(4));
    assert_eq!(policy.wait_duration(3), Duration::from_secs(8));
    assert_eq!(policy.wait_duration(4), Duration::from_secs(10));
    assert_eq!(policy.wait_duration(100), Duration::from_secs(10));
}

#[tokio::test]
async fn test_executor_success_no_retry() {
    let policy = RetryPolicy::new(5, Duration::from_millis(10));
    let stats = RetryStats::default();
    let executor = RetryExecutor::new(&policy, &stats);

    let result = executor
        .execute(|_attempt| async { Ok::<_, Aria2Error>(42u32) })
        .await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 42);
    assert_eq!(stats.total(), 0);
}

#[tokio::test]
async fn test_executor_retry_then_success() {
    let policy = RetryPolicy::new(5, Duration::from_millis(5));
    let stats = RetryStats::default();
    let executor = RetryExecutor::new(&policy, &stats);
    let call_count = Arc::new(std::sync::atomic::AtomicU32::new(0));

    let cc = call_count.clone();
    let result = executor
        .execute(move |attempt| {
            let cc = cc.clone();
            async move {
                cc.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if attempt < 2 {
                    Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: "retry me".into(),
                        },
                    ))
                } else {
                    Ok::<_, Aria2Error>("success")
                }
            }
        })
        .await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "success");
    assert_eq!(call_count.load(std::sync::atomic::Ordering::Relaxed), 3);
    assert_eq!(stats.total(), 2);
    assert_eq!(stats.network_failures(), 2);
}

#[tokio::test]
async fn test_executor_max_tries_exhausted() {
    let policy = RetryPolicy::new(3, Duration::from_millis(5));
    let stats = RetryStats::default();
    let executor = RetryExecutor::new(&policy, &stats);

    let result: Result<(), _> = executor
        .execute(|_attempt| async {
            Err(Aria2Error::Recoverable(RecoverableError::ServerError {
                code: 503,
            }))
        })
        .await;

    assert!(result.is_err());
    assert_eq!(stats.total(), 3);
    assert_eq!(stats.server_errors(), 3);
}

#[tokio::test]
async fn test_executor_fatal_error_no_retry() {
    let policy = RetryPolicy::new(5, Duration::from_millis(5));
    let stats = RetryStats::default();
    let executor = RetryExecutor::new(&policy, &stats);

    let result: Result<(), _> = executor
        .execute(|_attempt| async { Err(Aria2Error::Fatal(FatalError::DiskSpaceExhausted)) })
        .await;

    assert!(result.is_err());
    assert_eq!(stats.total(), 1);
}

#[test]
fn test_stats_reset() {
    let stats = RetryStats::default();
    stats.record_retry(&Aria2Error::Recoverable(RecoverableError::Timeout));
    stats.record_retry(&Aria2Error::Recoverable(RecoverableError::ServerError {
        code: 500,
    }));

    assert_eq!(stats.total(), 2);
    assert_eq!(stats.timeouts(), 1);
    assert_eq!(stats.server_errors(), 1);

    stats.reset();

    assert_eq!(stats.total(), 0);
    assert_eq!(stats.timeouts(), 0);
    assert_eq!(stats.server_errors(), 0);
}

#[test]
fn test_with_max_per_server() {
    let policy = RetryPolicy::new(10, Duration::from_secs(1)).with_max_per_server(3);
    assert_eq!(policy.max_tries(), 10);
}

#[tokio::test]
async fn test_concurrent_executors_independent() {
    let stats = Arc::new(RetryStats::default());

    let mut handles = Vec::new();
    for i in 0..10u32 {
        let stats_clone = stats.clone();
        handles.push(tokio::spawn(async move {
            let policy = RetryPolicy::new(3, Duration::from_millis(5));
            let executor = RetryExecutor::new(&policy, &stats_clone);
            let result: Result<u32, Aria2Error> = if i % 2 == 0 {
                executor.execute(|_| async { Ok(i) }).await
            } else {
                executor
                    .execute(|_| async { Err(Aria2Error::Recoverable(RecoverableError::Timeout)) })
                    .await
            };
            let _ = result;
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    assert!(stats.total() >= 10);
}
