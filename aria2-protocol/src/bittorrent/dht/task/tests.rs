use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A trivial test task that increments a counter.
#[derive(Debug)]
struct CountTask {
    counter: Arc<AtomicUsize>,
    name: &'static str,
}

#[async_trait::async_trait]
impl DhtTask for CountTask {
    async fn run(self: Box<Self>) {
        self.counter.fetch_add(1, Ordering::SeqCst);
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

#[tokio::test]
async fn test_executor_dispatches_task() {
    let executor = DhtTaskExecutor::new(2);
    let counter = Arc::new(AtomicUsize::new(0));

    executor
        .add_task(Box::new(CountTask {
            counter: Arc::clone(&counter),
            name: "test",
        }))
        .await;

    // Give the spawned task time to complete.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_executor_concurrency_limit() {
    let executor = DhtTaskExecutor::new(2);
    let counter = Arc::new(AtomicUsize::new(0));

    assert_eq!(executor.concurrency_limit(), 2);
    assert!(!executor.is_cancelled());

    // Enqueue 5 tasks — only 2 should execute concurrently.
    for _ in 0..5 {
        executor
            .add_task(Box::new(CountTask {
                counter: Arc::clone(&counter),
                name: "concurrent-test",
            }))
            .await;
    }

    // Wait for all tasks to complete.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert_eq!(counter.load(Ordering::SeqCst), 5);
    assert_eq!(executor.executing_count().await, 0);

    executor.cancel();
    assert!(executor.is_cancelled());
    assert!(
        !executor
            .add_task(Box::new(CountTask {
                counter,
                name: "cancelled",
            }))
            .await
    );
}

#[tokio::test]
async fn test_task_queue_three_lanes() {
    let queue = DhtTaskQueue::new();
    let c1 = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::new(AtomicUsize::new(0));
    let c3 = Arc::new(AtomicUsize::new(0));

    queue
        .add_periodic_task_1(Box::new(CountTask {
            counter: Arc::clone(&c1),
            name: "p1",
        }))
        .await;
    queue
        .add_periodic_task_2(Box::new(CountTask {
            counter: Arc::clone(&c2),
            name: "p2",
        }))
        .await;
    queue
        .add_immediate_task(Box::new(CountTask {
            counter: Arc::clone(&c3),
            name: "imm",
        }))
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert_eq!(c1.load(Ordering::SeqCst), 1);
    assert_eq!(c2.load(Ordering::SeqCst), 1);
    assert_eq!(c3.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_task_queue_lanes_have_independent_capacity() {
    let queue = DhtTaskQueue::with_concurrency(1);
    let periodic_started = Arc::new(tokio::sync::Notify::new());
    let release_periodic = Arc::new(tokio::sync::Notify::new());

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
            "blocking-periodic"
        }
    }

    queue
        .add_periodic_task_1(Box::new(BlockingTask {
            started: Arc::clone(&periodic_started),
            release: Arc::clone(&release_periodic),
        }))
        .await;
    let periodic_started = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        periodic_started.notified(),
    )
    .await
    .is_ok();

    let immediate_count = Arc::new(AtomicUsize::new(0));
    queue
        .add_immediate_task(Box::new(CountTask {
            counter: Arc::clone(&immediate_count),
            name: "immediate",
        }))
        .await;
    let immediate_ran = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while immediate_count.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();

    release_periodic.notify_one();
    queue.shutdown().await;

    assert!(periodic_started, "periodic task should start");
    assert!(
        immediate_ran,
        "an occupied periodic lane must not block the immediate lane"
    );
}

#[tokio::test]
async fn test_executor_queue_size() {
    let executor = DhtTaskExecutor::new(1);
    let counter = Arc::new(AtomicUsize::new(0));

    // A slow task that holds the slot.
    #[derive(Debug)]
    struct SlowTask {
        counter: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl DhtTask for SlowTask {
        async fn run(self: Box<Self>) {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            self.counter.fetch_add(1, Ordering::SeqCst);
        }

        fn name(&self) -> &'static str {
            "slow"
        }
    }

    executor
        .add_task(Box::new(SlowTask {
            counter: Arc::clone(&counter),
        }))
        .await;

    // Add more tasks while the first is running.
    for _ in 0..3 {
        executor
            .add_task(Box::new(CountTask {
                counter: Arc::clone(&counter),
                name: "queued",
            }))
            .await;
    }

    // Should have 1 executing + some queued.
    let executing = executor.executing_count().await;
    let queued = executor.queue_size().await;
    assert!(
        executing + queued >= 3,
        "executing={}, queued={}",
        executing,
        queued
    );

    // Wait for all to finish.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(counter.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn test_executor_coalesces_periodic_work_while_busy() {
    let executor = DhtTaskExecutor::new(1);
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let counter = Arc::new(AtomicUsize::new(0));

    #[derive(Debug)]
    struct BlockingTask {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        counter: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl DhtTask for BlockingTask {
        async fn run(self: Box<Self>) {
            self.counter.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            self.release.notified().await;
        }

        fn name(&self) -> &'static str {
            "blocking"
        }
    }

    executor
        .add_task(Box::new(BlockingTask {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
            counter: Arc::clone(&counter),
        }))
        .await;
    started.notified().await;

    assert!(
        !executor
            .try_add_task_if_idle(Box::new(CountTask {
                counter: Arc::clone(&counter),
                name: "coalesced",
            }))
            .await
    );
    assert_eq!(executor.queue_size().await, 0);

    release.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_executor_shutdown_cancels_running_task() {
    let executor = DhtTaskExecutor::new(1);
    let started = Arc::new(tokio::sync::Notify::new());
    let counter = Arc::new(AtomicUsize::new(0));

    #[derive(Debug)]
    struct NeverEndingTask {
        started: Arc<tokio::sync::Notify>,
        counter: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl DhtTask for NeverEndingTask {
        async fn run(self: Box<Self>) {
            self.counter.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            std::future::pending::<()>().await;
        }

        fn name(&self) -> &'static str {
            "never-ending"
        }
    }

    executor
        .add_task(Box::new(NeverEndingTask {
            started: Arc::clone(&started),
            counter: Arc::clone(&counter),
        }))
        .await;
    started.notified().await;

    executor.shutdown().await;
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert_eq!(executor.executing_count().await, 0);
    assert!(
        !executor
            .try_add_task_if_idle(Box::new(CountTask {
                counter: Arc::clone(&counter),
                name: "after-shutdown",
            }))
            .await
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_executor_does_not_stall_after_task_panic() {
    #[derive(Debug)]
    struct PanicTask {
        started: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl DhtTask for PanicTask {
        async fn run(self: Box<Self>) {
            self.started.notify_one();
            panic!("intentional task panic");
        }

        fn name(&self) -> &'static str {
            "panic"
        }
    }

    let executor = DhtTaskExecutor::new(1);
    let started = Arc::new(tokio::sync::Notify::new());
    assert!(
        executor
            .add_task(Box::new(PanicTask {
                started: Arc::clone(&started),
            }))
            .await
    );
    started.notified().await;

    let shutdown =
        tokio::time::timeout(std::time::Duration::from_millis(500), executor.shutdown()).await;
    assert!(shutdown.is_ok(), "executor shutdown should not stall");
    assert_eq!(executor.executing_count().await, 0);
}
