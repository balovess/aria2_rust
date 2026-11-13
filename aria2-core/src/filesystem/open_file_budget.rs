use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

#[derive(Debug)]
struct State {
    max_open_files: usize,
    reserved: usize,
}

/// Process-wide budget for file handles opened by multi-file download writers.
#[derive(Debug)]
pub(crate) struct OpenFileBudget {
    state: Mutex<State>,
    changed: Notify,
}

impl OpenFileBudget {
    pub(crate) fn new(max_open_files: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                max_open_files: max_open_files.max(1),
                reserved: 0,
            }),
            changed: Notify::new(),
        })
    }

    pub(crate) fn set_max_open_files(&self, max_open_files: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.max_open_files = max_open_files.max(1);
        drop(state);
        self.changed.notify_waiters();
    }

    pub(crate) fn max_open_files(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .max_open_files
    }

    /// Reserve up to `requested` file handles, waiting only when the process
    /// budget is fully occupied. A smaller reservation lets callers split a
    /// batch without holding partial permits while waiting for more.
    pub(crate) async fn reserve_up_to(self: &Arc<Self>, requested: usize) -> OpenFileReservation {
        debug_assert!(requested > 0);

        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let permits = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let available = state.max_open_files.saturating_sub(state.reserved);
                let permits = requested.min(available);
                if permits > 0 {
                    state.reserved += permits;
                    Some(permits)
                } else {
                    None
                }
            };

            if let Some(permits) = permits {
                return OpenFileReservation {
                    budget: Arc::clone(self),
                    permits,
                };
            }

            notified.await;
        }
    }

    fn release(&self, permits: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.reserved = state.reserved.saturating_sub(permits);
        drop(state);
        self.changed.notify_waiters();
    }
}

pub(crate) struct OpenFileReservation {
    budget: Arc<OpenFileBudget>,
    permits: usize,
}

impl OpenFileReservation {
    pub(crate) fn permits(&self) -> usize {
        self.permits
    }
}

impl Drop for OpenFileReservation {
    fn drop(&mut self) {
        self.budget.release(self.permits);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::OpenFileBudget;

    #[tokio::test]
    async fn reservations_share_the_process_limit_and_release_on_drop() {
        let budget = OpenFileBudget::new(3);
        let first = budget.reserve_up_to(2).await;
        let second = budget.reserve_up_to(2).await;

        assert_eq!(first.permits(), 2);
        assert_eq!(second.permits(), 1);
        drop(first);

        let third = budget.reserve_up_to(2).await;
        assert_eq!(third.permits(), 2);
    }

    #[tokio::test]
    async fn waiters_resume_when_the_limit_is_raised() {
        let budget = OpenFileBudget::new(1);
        let held = budget.reserve_up_to(1).await;
        let waiting_budget = Arc::clone(&budget);
        let waiter = tokio::spawn(async move { waiting_budget.reserve_up_to(1).await });

        tokio::task::yield_now().await;
        budget.set_max_open_files(2);
        let reservation = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("raised budget should wake a waiter")
            .expect("waiter task should complete");

        assert_eq!(reservation.permits(), 1);
        drop(held);
    }
}
