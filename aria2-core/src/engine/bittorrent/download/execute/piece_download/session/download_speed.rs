use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use crate::engine::bittorrent::peer::stats::SpeedWindow;
use crate::request::request_group::AtomicProgress;

pub(super) const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

struct DownloadSpeedSampler {
    last_sample: Instant,
    window: SpeedWindow,
}

impl DownloadSpeedSampler {
    fn new(now: Instant) -> Self {
        Self {
            last_sample: now,
            window: SpeedWindow::default(),
        }
    }

    fn record(&mut self, bytes: u64, now: Instant) {
        if bytes == 0 {
            return;
        }

        self.window.record(bytes, now);
    }

    fn speed_at(&mut self, now: Instant) -> u64 {
        self.last_sample = now;
        self.window.speed_at(now)
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.window
            .has_samples_at(self.last_sample)
            .then_some(self.last_sample + SAMPLE_INTERVAL)
    }
}

pub(super) fn spawn(progress: Arc<AtomicProgress>) -> tokio::task::JoinHandle<()> {
    let signal = progress.download_rate_signal();
    tokio::spawn(async move {
        let mut sampler = DownloadSpeedSampler::new(Instant::now());
        loop {
            let notified = signal.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let now = Instant::now();
            let bytes = progress.take_download_payload_bytes();
            sampler.record(bytes, now);
            progress.set_download_speed(sampler.speed_at(now));

            if let Some(deadline) = sampler.next_deadline() {
                tokio::select! {
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {}
                    _ = &mut notified => {}
                }
            } else {
                notified.await;
            }
        }
    })
}
