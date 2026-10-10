use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

use crate::engine::command::{ProgressMessage, ProgressUpdate};
use crate::request::global_net_stat::GlobalNetStat;
use crate::request::request_group::AtomicProgress;
use crate::util::perf_monitor::{AtomicMetrics, PerformanceMonitor};

pub struct ProgressUpdater {
    progress_sender: Option<mpsc::Sender<ProgressMessage>>,
    global_net_stat: Option<Arc<GlobalNetStat>>,
    /// Direct access to progress counters — avoids `RwLock` on the hot path.
    progress: Arc<AtomicProgress>,
    atomic_metrics: Arc<AtomicMetrics>,
    perf_monitor: Option<Arc<PerformanceMonitor>>,
    last_speed_update: Instant,
    last_completed: u64,
    last_progress_update: u64,
    last_global_progress: u64,
}

impl Clone for ProgressUpdater {
    fn clone(&self) -> Self {
        Self {
            progress_sender: self.progress_sender.clone(),
            global_net_stat: self.global_net_stat.clone(),
            progress: Arc::clone(&self.progress),
            atomic_metrics: Arc::clone(&self.atomic_metrics),
            perf_monitor: self.perf_monitor.clone(),
            last_speed_update: self.last_speed_update,
            last_completed: self.last_completed,
            last_progress_update: self.last_progress_update,
            last_global_progress: self.last_global_progress,
        }
    }
}

impl ProgressUpdater {
    pub(crate) fn new(
        progress_sender: Option<mpsc::Sender<ProgressMessage>>,
        global_net_stat: Option<Arc<GlobalNetStat>>,
        progress: Arc<AtomicProgress>,
        atomic_metrics: Arc<AtomicMetrics>,
        perf_monitor: Option<Arc<PerformanceMonitor>>,
    ) -> Self {
        Self {
            progress_sender,
            global_net_stat,
            progress,
            atomic_metrics,
            perf_monitor,
            last_speed_update: Instant::now(),
            last_completed: 0,
            last_progress_update: 0,
            last_global_progress: 0,
        }
    }

    pub fn reset(&mut self, completed_bytes: u64) {
        self.last_speed_update = Instant::now();
        self.last_completed = completed_bytes;
        self.last_progress_update = completed_bytes;
        self.last_global_progress = completed_bytes;
    }

    pub async fn update_progress(
        &mut self,
        completed_bytes: u64,
        progress_update_threshold: u64,
        speed_update_interval_ms: u64,
    ) {
        self.update_progress_inner(
            completed_bytes,
            progress_update_threshold,
            speed_update_interval_ms,
            true,
        )
        .await;
    }

    /// Update progress accounting when another component owns the live speed
    /// estimate. The channel receives length-only messages and preserves the
    /// directly published speed value.
    pub(crate) async fn update_progress_without_speed(
        &mut self,
        completed_bytes: u64,
        progress_update_threshold: u64,
        speed_update_interval_ms: u64,
    ) {
        self.update_progress_inner(
            completed_bytes,
            progress_update_threshold,
            speed_update_interval_ms,
            false,
        )
        .await;
    }

    async fn update_progress_inner(
        &mut self,
        completed_bytes: u64,
        progress_update_threshold: u64,
        speed_update_interval_ms: u64,
        publish_speed: bool,
    ) {
        if completed_bytes > self.last_global_progress {
            let delta = completed_bytes - self.last_global_progress;
            if let Some(global) = self.global_net_stat.as_ref() {
                global.update_download(delta);
            }
            self.last_global_progress = completed_bytes;
        }

        if completed_bytes.saturating_sub(self.last_progress_update) < progress_update_threshold {
            return;
        }

        let elapsed = self.last_speed_update.elapsed();
        let speed = if elapsed.as_millis() >= speed_update_interval_ms as u128 {
            let delta = completed_bytes.saturating_sub(self.last_completed);
            let s = (delta as f64 / elapsed.as_secs_f64()) as u64;
            self.last_speed_update = Instant::now();
            self.last_completed = completed_bytes;
            s
        } else {
            0
        };

        if let Some(ref sender) = self.progress_sender {
            let update = ProgressUpdate {
                completed_bytes,
                download_speed: speed,
                upload_speed: 0,
            };
            let message = if publish_speed {
                ProgressMessage::Update(update)
            } else {
                ProgressMessage::LengthOnly(update)
            };
            let _ = sender.send(message).await;
        } else {
            self.progress.set_completed_length(completed_bytes);
            if publish_speed && speed > 0 {
                self.progress.set_download_speed(speed);
                self.progress.set_upload_speed(0);
            }
        }

        if speed > 0 {
            self.atomic_metrics.record_throughput(speed);
            if let Some(ref monitor) = self.perf_monitor {
                let metrics = crate::util::perf_monitor::Metrics::new(
                    speed,
                    elapsed.as_millis() as u64,
                    0,
                    0,
                )
                .with_label("download_speed");
                monitor.record_metric("download_speed", metrics);
            }
        }

        self.last_progress_update = completed_bytes;
    }

    pub fn last_progress_update(&self) -> u64 {
        self.last_progress_update
    }
}

#[cfg(test)]
mod tests {
    use super::ProgressUpdater;
    use crate::engine::command::ProgressMessage;
    use crate::request::global_net_stat::GlobalNetStat;
    use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
    use crate::util::rwlock_ext::RwLockRecover;
    use std::sync::Arc;

    #[tokio::test]
    async fn restored_offset_is_not_counted_as_new_global_download_bytes() {
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(1),
            Vec::new(),
            DownloadOptions::default(),
        )));
        let global = Arc::new(GlobalNetStat::default());
        group.recover_mut().set_global_net_stat(Arc::clone(&global));
        let progress = group.recover().progress.clone();
        let mut updater = ProgressUpdater::new(
            None,
            group.recover().global_net_stat(),
            progress,
            Arc::new(crate::util::perf_monitor::AtomicMetrics::new()),
            None,
        );

        updater.reset(1024);
        updater.update_progress(1100, 4096, 1_000).await;
        updater.update_progress(1100, 4096, 1_000).await;
        updater.update_progress(1200, 4096, 1_000).await;

        assert_eq!(global.session_download_length_for_test(), 176);
    }

    #[tokio::test]
    async fn external_speed_updates_do_not_publish_a_second_speed_sample() {
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(1),
            Vec::new(),
            DownloadOptions::default(),
        )));
        let global = Arc::new(GlobalNetStat::default());
        group.recover_mut().set_global_net_stat(Arc::clone(&global));
        let progress = group.recover().progress.clone();
        progress.set_download_speed(12_345);
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let mut updater = ProgressUpdater::new(
            Some(sender),
            group.recover().global_net_stat(),
            Arc::clone(&progress),
            Arc::new(crate::util::perf_monitor::AtomicMetrics::new()),
            None,
        );

        updater
            .update_progress_without_speed(4096, 1, u64::MAX)
            .await;

        assert!(matches!(
            receiver.try_recv(),
            Ok(ProgressMessage::LengthOnly(update)) if update.completed_bytes == 4096
        ));
        assert_eq!(progress.download_speed(), 12_345);
        assert_eq!(global.session_download_length_for_test(), 4096);
        assert_eq!(updater.last_progress_update(), 4096);
    }
}
