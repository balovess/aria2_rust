//! Unified deadline-driven coordinator for aria2's two auto-save features.
//!
//! The features share one engine wake-up path, but they intentionally keep
//! different persistence semantics:
//! - `auto-save-interval` requests protocol-owned `*.aria2` checkpoints.
//! - `save-session-interval` serializes the configured session file.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::debug;

use super::auto_save_session::AutoSaveSession;
use crate::request::request_group_man::RequestGroupMan;

pub struct AutoSaveCoordinator {
    request_group_man: Arc<RequestGroupMan>,
    session: Option<AutoSaveSession>,
    control_file_interval: Option<Duration>,
    last_control_file_save: Instant,
}

impl AutoSaveCoordinator {
    pub fn new(
        request_group_man: Arc<RequestGroupMan>,
        session: Option<(PathBuf, Duration)>,
        control_file_interval: Option<Duration>,
    ) -> Self {
        let session = session.map(|(path, interval)| {
            AutoSaveSession::new(path, interval, Arc::clone(&request_group_man))
        });
        Self {
            request_group_man,
            session,
            control_file_interval,
            last_control_file_save: Instant::now(),
        }
    }

    pub fn mark_session_dirty(&self) {
        if let Some(session) = &self.session {
            session.mark_dirty();
        }
    }

    pub fn is_session_dirty(&self) -> bool {
        self.session.as_ref().is_some_and(AutoSaveSession::is_dirty)
    }

    /// Return the earliest real persistence deadline.
    pub fn next_deadline(&self, has_pending_downloads: bool) -> Option<Instant> {
        let session_deadline = self.session.as_ref().and_then(|session| {
            if session.interval().is_zero() {
                session.is_dirty().then_some(Instant::now())
            } else {
                (has_pending_downloads || session.is_dirty())
                    .then_some(session.next_save_deadline())
            }
        });

        let control_file_deadline = self.control_file_interval.and_then(|interval| {
            (!interval.is_zero() && has_pending_downloads)
                .then_some(self.last_control_file_save + interval)
        });

        [session_deadline, control_file_deadline]
            .into_iter()
            .flatten()
            .min()
    }

    /// Execute every persistence action whose deadline has elapsed.
    pub async fn run_due(&mut self, has_pending_downloads: bool) {
        let now = Instant::now();

        if let Some(interval) = self.control_file_interval
            && !interval.is_zero()
            && has_pending_downloads
            && now >= self.last_control_file_save + interval
        {
            self.request_group_man.request_control_file_saves();
            self.last_control_file_save = now;
            debug!(
                interval_seconds = interval.as_secs_f64(),
                "Requested periodic control-file saves"
            );
        }

        if let Some(session) = &mut self.session {
            if has_pending_downloads && !session.interval().is_zero() {
                session.mark_dirty();
            }
            let due = if session.interval().is_zero() {
                session.is_dirty()
            } else {
                now >= session.next_save_deadline()
            };
            if due {
                session.save_if_dirty().await;
            }
        }
    }

    /// Save all configured state during engine shutdown.
    pub async fn force_save(&mut self) {
        self.request_group_man.request_control_file_saves();
        if let Some(session) = &mut self.session {
            session.force_save().await;
        }
    }

    /// Test and focused-call helper for the session sub-path.
    pub async fn save_if_dirty(&mut self) {
        if let Some(session) = &mut self.session {
            session.save_if_dirty().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::request_group::DownloadOptions;
    use crate::util::rwlock_ext::RwLockRecover;

    #[tokio::test]
    async fn session_and_control_file_saves_share_deadline_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let session_path = dir.path().join("session.txt");
        let man = Arc::new(RequestGroupMan::new());
        let gid = man
            .add_group(
                vec!["http://example.com/coordinated.bin".into()],
                DownloadOptions::default(),
            )
            .unwrap();
        let mut coordinator = AutoSaveCoordinator::new(
            Arc::clone(&man),
            Some((session_path.clone(), Duration::ZERO)),
            Some(Duration::from_secs(60)),
        );
        coordinator.last_control_file_save = Instant::now() - Duration::from_secs(61);
        coordinator.mark_session_dirty();

        coordinator.run_due(true).await;

        assert!(session_path.exists());
        assert!(
            man.find_group(gid)
                .unwrap()
                .recover()
                .is_save_control_file_requested()
        );
    }

    #[tokio::test]
    async fn zero_control_file_interval_only_saves_on_force() {
        let man = Arc::new(RequestGroupMan::new());
        let gid = man
            .add_group(
                vec!["http://example.com/coordinated-zero.bin".into()],
                DownloadOptions::default(),
            )
            .unwrap();
        let mut coordinator =
            AutoSaveCoordinator::new(Arc::clone(&man), None, Some(Duration::ZERO));

        coordinator.run_due(true).await;
        assert!(
            !man.find_group(gid)
                .unwrap()
                .recover()
                .is_save_control_file_requested()
        );

        coordinator.force_save().await;
        assert!(
            man.find_group(gid)
                .unwrap()
                .recover()
                .is_save_control_file_requested()
        );
    }

    #[tokio::test]
    async fn zero_session_interval_does_not_create_a_busy_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let session_path = dir.path().join("zero-session.txt");
        let man = Arc::new(RequestGroupMan::new());
        man.add_group(
            vec!["http://example.com/zero-session.bin".into()],
            DownloadOptions::default(),
        )
        .unwrap();
        let mut coordinator =
            AutoSaveCoordinator::new(man, Some((session_path, Duration::ZERO)), None);

        coordinator.mark_session_dirty();
        assert!(coordinator.next_deadline(true).is_some());
        coordinator.run_due(true).await;
        assert!(
            coordinator.next_deadline(true).is_none(),
            "zero interval must not reschedule itself while downloads are active"
        );
    }
}
