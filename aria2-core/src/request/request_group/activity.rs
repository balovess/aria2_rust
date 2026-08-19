//! Event signal for observers of request-group activity.

use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Notify;

/// A lossless wake-up edge for consumers that read a changing snapshot.
///
/// `Notify` alone can lose a wake-up when a consumer is rendering a snapshot.
/// The generation counter makes the signal level-sensitive: a consumer can
/// compare its last observed generation after every render and never needs a
/// fixed-rate wake-up timer.
pub struct ActivitySignal {
    generation: AtomicU64,
    notify: Notify,
}

impl ActivitySignal {
    pub fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            notify: Notify::new(),
        }
    }

    /// Publish one or more changes to the observed snapshot.
    pub fn notify(&self) {
        self.generation.fetch_add(1, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Return the current change generation.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Wait until the generation differs from `observed` and update it.
    pub async fn wait_for_change(&self, observed: &mut u64) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let current = self.generation();
            if current != *observed {
                *observed = current;
                return;
            }

            notified.await;
        }
    }
}

impl Default for ActivitySignal {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::ActivitySignal;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn wait_for_change_observes_events_published_before_waiting() {
        let signal = Arc::new(ActivitySignal::new());
        let mut observed = signal.generation();
        signal.notify();

        tokio::time::timeout(
            Duration::from_secs(1),
            signal.wait_for_change(&mut observed),
        )
        .await
        .expect("published activity must wake the observer");

        assert_eq!(observed, signal.generation());
    }

    #[tokio::test]
    async fn wait_for_change_does_not_complete_without_an_event() {
        let signal = ActivitySignal::new();
        let mut observed = signal.generation();

        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                signal.wait_for_change(&mut observed),
            )
            .await
            .is_err()
        );
    }
}
