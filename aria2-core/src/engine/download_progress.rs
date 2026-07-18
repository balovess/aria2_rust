use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

use crate::engine::command::ProgressUpdate;
use crate::request::request_group::RequestGroup;
use crate::util::perf_monitor::{AtomicMetrics, PerformanceMonitor};

pub struct ProgressUpdater {
    progress_sender: Option<mpsc::UnboundedSender<ProgressUpdate>>,
    group: Arc<tokio::sync::RwLock<RequestGroup>>,
    atomic_metrics: Arc<AtomicMetrics>,
    perf_monitor: Option<Arc<PerformanceMonitor>>,
    last_speed_update: Instant,
    last_completed: u64,
    last_progress_update: u64,
}

impl Clone for ProgressUpdater {
    fn clone(&self) -> Self {
        Self {
            progress_sender: self.progress_sender.clone(),
            group: Arc::clone(&self.group),
            atomic_metrics: Arc::clone(&self.atomic_metrics),
            perf_monitor: self.perf_monitor.clone(),
            last_speed_update: self.last_speed_update,
            last_completed: self.last_completed,
            last_progress_update: self.last_progress_update,
        }
    }
}

impl ProgressUpdater {
    pub fn new(
        progress_sender: Option<mpsc::UnboundedSender<ProgressUpdate>>,
        group: Arc<tokio::sync::RwLock<RequestGroup>>,
        atomic_metrics: Arc<AtomicMetrics>,
        perf_monitor: Option<Arc<PerformanceMonitor>>,
    ) -> Self {
        Self {
            progress_sender,
            group,
            atomic_metrics,
            perf_monitor,
            last_speed_update: Instant::now(),
            last_completed: 0,
            last_progress_update: 0,
        }
    }

    pub fn reset(&mut self, completed_bytes: u64) {
        self.last_speed_update = Instant::now();
        self.last_completed = completed_bytes;
        self.last_progress_update = completed_bytes;
    }

    pub async fn update_progress(
        &mut self,
        completed_bytes: u64,
        progress_update_threshold: u64,
        speed_update_interval_ms: u64,
    ) {
        if completed_bytes - self.last_progress_update < progress_update_threshold {
            return;
        }

        let elapsed = self.last_speed_update.elapsed();
        let speed = if elapsed.as_millis() >= speed_update_interval_ms as u128 {
            let delta = completed_bytes - self.last_completed;
            let s = (delta as f64 / elapsed.as_secs_f64()) as u64;
            self.last_speed_update = Instant::now();
            self.last_completed = completed_bytes;
            s
        } else {
            0
        };

        if let Some(ref sender) = self.progress_sender {
            let _ = sender.send(ProgressUpdate {
                completed_bytes,
                download_speed: speed,
                upload_speed: 0,
            });
        } else {
            let g = self.group.write().await;
            g.update_progress(completed_bytes).await;
            g.set_completed_length(completed_bytes);
            if speed > 0 {
                g.update_speed(speed, 0).await;
                g.set_download_speed_cached(speed);
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
