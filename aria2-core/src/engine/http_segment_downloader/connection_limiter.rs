//! Connection limiting for HTTP segmented downloads.
//!
//! Tracks active connections per hostname to enforce max-connection-per-server
//! limits using lock-free atomics and `DashMap` for interior mutability.

use dashmap::DashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Track active connections per hostname to enforce max-connection-per-server limit.
///
/// Uses `DashMap` + `AtomicUsize` for interior mutability, so every method takes
/// `&self` and the limiter can be shared via `Arc<ConnectionLimiter>` without any
/// wrapping `Mutex`/`RwLock`. This is a *soft* limiter: the CAS-with-rollback
/// pattern in [`ConnectionLimiter::try_acquire`] keeps the global and per-host
/// counts bounded by their limits at the moment of each successful CAS, but
/// snapshot readers (e.g. [`ConnectionLimiter::host_count`]) may observe
/// briefly-stale values.
pub struct ConnectionLimiter {
    per_host: DashMap<String, AtomicUsize>,
    global_count: AtomicUsize,
    global_limit: usize,
    per_host_limit: usize,
}

impl ConnectionLimiter {
    /// Create a new `ConnectionLimiter` with the given global and per-host limits.
    pub fn new(global: usize, per_host: usize) -> Self {
        Self {
            per_host: DashMap::new(),
            global_count: AtomicUsize::new(0),
            global_limit: global,
            per_host_limit: per_host,
        }
    }

    /// Try to acquire a connection slot for the given host.
    ///
    /// Returns `true` if the slot was acquired, `false` if either the global or
    /// the per-host limit has been reached.
    ///
    /// # Algorithm (CAS-with-rollback)
    ///
    /// 1. Atomically increment `global_count` via CAS; abort if at/above limit.
    /// 2. Atomically increment the per-host counter via CAS; if the per-host
    ///    limit is hit, roll back the global increment so `global_count` stays
    ///    accurate.
    ///
    /// This guarantees `global_count` never exceeds `global_limit` at the
    /// moment of a successful CAS, and each host's counter never exceeds
    /// `per_host_limit`. A brief shard-level write lock is held by the
    /// `DashMap` `Entry` guard during the per-host CAS — acceptable for a
    /// connection limiter which is not a hot path.
    pub fn try_acquire(&self, host: &str) -> bool {
        // Step 1: Atomically acquire a global slot via CAS. Only threads that
        // observe `current < global_limit` can succeed, so the global count can
        // never exceed `global_limit` at the instant of a successful CAS.
        loop {
            let current = self.global_count.load(Ordering::Relaxed);
            if current >= self.global_limit {
                return false;
            }
            match self.global_count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(_) => continue,
            }
        }

        // Step 2: Try to acquire a per-host slot. On failure, roll back the
        // global increment performed above so the global count stays accurate.
        // The rollback uses Relaxed ordering: it only needs to eventually
        // correct the over-count introduced in step 1, which was already
        // published with AcqRel. A short-lived false-positive on the global
        // limit (another thread briefly sees the inflated count) is acceptable
        // for a soft limiter.
        let entry = self
            .per_host
            .entry(host.to_string())
            .or_insert_with(|| AtomicUsize::new(0));
        loop {
            let current = entry.load(Ordering::Relaxed);
            if current >= self.per_host_limit {
                // Per-host limit reached — roll back the global increment.
                self.global_count.fetch_sub(1, Ordering::Relaxed);
                return false;
            }
            match entry.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(_) => continue,
            }
        }
    }

    /// Release a previously-acquired connection slot for the given host.
    ///
    /// The caller must call this exactly once per successful `try_acquire` for
    /// the same host. Double-releases are detected via `debug_assert!` in debug
    /// builds.
    pub fn release(&self, host: &str) {
        if let Some(entry) = self.per_host.get(host) {
            let prev = entry.fetch_sub(1, Ordering::AcqRel);
            debug_assert!(prev > 0, "release called more times than acquire for host");
        }
        let prev = self.global_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0, "global release underflow");
    }

    /// Current connection count for a host (snapshot — may be slightly stale).
    pub fn host_count(&self, host: &str) -> usize {
        self.per_host
            .get(host)
            .map(|e| e.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Total connection count across all hosts (snapshot — may be slightly stale).
    pub fn global_count(&self) -> usize {
        self.global_count.load(Ordering::Relaxed)
    }

    /// The configured global connection limit.
    pub fn global_limit(&self) -> usize {
        self.global_limit
    }

    /// The configured per-host connection limit.
    pub fn per_host_limit(&self) -> usize {
        self.per_host_limit
    }

    /// How many slots are still available for the given host (snapshot).
    ///
    /// Returns 0 if the host is at or above its per-host limit.
    pub fn available_for(&self, host: &str) -> usize {
        let current = self.host_count(host);
        if current >= self.per_host_limit {
            return 0;
        }
        self.per_host_limit - current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_limiter_per_host() {
        let limiter = ConnectionLimiter::new(10, 2); // Global limit 10, per-host limit 2

        // Should be able to acquire up to per_host_limit
        assert!(
            limiter.try_acquire("example.com"),
            "First acquisition should succeed"
        );
        assert!(
            limiter.try_acquire("example.com"),
            "Second acquisition should succeed"
        );
        assert!(
            !limiter.try_acquire("example.com"),
            "Third acquisition should fail (per-host limit)"
        );

        // Different host should work independently
        assert!(
            limiter.try_acquire("other.com"),
            "Different host should work"
        );
        assert!(
            limiter.try_acquire("other.com"),
            "Second slot for other host"
        );
        assert!(
            !limiter.try_acquire("other.com"),
            "Third slot for other host should fail"
        );

        // Release a slot
        limiter.release("example.com");
        assert!(
            limiter.try_acquire("example.com"),
            "After release, should acquire again"
        );

        // Check available slots
        assert_eq!(
            limiter.available_for("example.com"),
            0,
            "No slots available after acquiring limit"
        );
        limiter.release("example.com");
        assert_eq!(
            limiter.available_for("example.com"),
            1,
            "One slot available after release"
        );
    }

    /// Verify the limiter is safe under concurrency: 20 tasks contend on a
    /// single host whose per-host limit is 10, so exactly 10 must succeed and
    /// the limiter must not deadlock. Also verifies `release` correctness.
    #[tokio::test]
    async fn test_connection_limiter_concurrent_no_deadlock() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let limiter = Arc::new(ConnectionLimiter::new(100, 10));
        let success_count = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..20 {
            let l = limiter.clone();
            let s = success_count.clone();
            handles.push(tokio::spawn(async move {
                // `try_acquire` is fully synchronous — no `.await` while holding
                // the DashMap shard guard, so there is no risk of deadlock.
                if l.try_acquire("example.com") {
                    s.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }

        for h in handles {
            h.await.expect("task should not panic");
        }

        // per_host_limit is 10, so at most 10 should succeed; global limit (100)
        // is not the binding constraint here.
        assert_eq!(
            success_count.load(Ordering::Relaxed),
            10,
            "per-host limit must cap successful acquires"
        );
        assert_eq!(
            limiter.host_count("example.com"),
            10,
            "host_count must reflect the 10 successful acquires"
        );
        assert_eq!(
            limiter.global_count(),
            10,
            "global_count must equal the 10 successful acquires"
        );

        // Release some and verify the counts drop accordingly.
        for _ in 0..5 {
            limiter.release("example.com");
        }
        assert_eq!(
            limiter.host_count("example.com"),
            5,
            "host_count must drop to 5 after 5 releases"
        );
        assert_eq!(
            limiter.global_count(),
            5,
            "global_count must drop to 5 after 5 releases"
        );
    }
}
