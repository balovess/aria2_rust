use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::request::request_group::AtomicProgress;

pub(super) const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);
const SPEED_WINDOW: Duration = Duration::from_secs(10);
const SPEED_SLOT: Duration = Duration::from_secs(1);

struct DownloadSpeedSampler {
    last_sample: Instant,
    samples: VecDeque<(Instant, u64)>,
    window_bytes: u64,
}

impl DownloadSpeedSampler {
    fn new(now: Instant) -> Self {
        Self {
            last_sample: now,
            samples: VecDeque::new(),
            window_bytes: 0,
        }
    }

    fn record(&mut self, bytes: u64, now: Instant) {
        if bytes == 0 {
            return;
        }

        if let Some((slot_time, slot_bytes)) = self.samples.back_mut()
            && now.saturating_duration_since(*slot_time) < SPEED_SLOT
        {
            *slot_bytes = slot_bytes.saturating_add(bytes);
        } else {
            self.samples.push_back((now, bytes));
        }
        self.window_bytes = self.window_bytes.saturating_add(bytes);
    }

    fn speed_at(&mut self, now: Instant) -> u64 {
        while self.samples.front().is_some_and(|(sampled_at, _)| {
            now.saturating_duration_since(*sampled_at) > SPEED_WINDOW
        }) {
            if let Some((_, bytes)) = self.samples.pop_front() {
                self.window_bytes = self.window_bytes.saturating_sub(bytes);
            }
        }

        self.last_sample = now;
        let Some((oldest_sample, _)) = self.samples.front() else {
            return 0;
        };
        let elapsed_millis = now
            .saturating_duration_since(*oldest_sample)
            .as_millis()
            .max(1);
        (u128::from(self.window_bytes) * 1_000 / elapsed_millis).min(u128::from(u64::MAX)) as u64
    }

    fn next_deadline(&self) -> Option<Instant> {
        (!self.samples.is_empty()).then_some(self.last_sample + SAMPLE_INTERVAL)
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
