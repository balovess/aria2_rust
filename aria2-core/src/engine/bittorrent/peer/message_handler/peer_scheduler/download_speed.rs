use std::time::{Duration, Instant};

pub(super) const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

pub(super) struct DownloadSpeedSampler {
    last_sample: Instant,
    received_bytes: u64,
    sampled_idle: bool,
}

impl DownloadSpeedSampler {
    pub(super) fn new() -> Self {
        Self::at(Instant::now())
    }

    fn at(now: Instant) -> Self {
        Self {
            last_sample: now,
            received_bytes: 0,
            sampled_idle: false,
        }
    }

    pub(super) fn record(&mut self, bytes: u64) {
        self.received_bytes = self.received_bytes.saturating_add(bytes);
        self.sampled_idle = false;
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        (!self.sampled_idle || self.received_bytes > 0)
            .then_some(self.last_sample + SAMPLE_INTERVAL)
    }

    pub(super) fn sample(&mut self, now: Instant) -> u64 {
        let elapsed = now.saturating_duration_since(self.last_sample);
        let speed = if elapsed.is_zero() {
            0
        } else {
            (self.received_bytes as f64 / elapsed.as_secs_f64()) as u64
        };
        self.last_sample = now;
        self.sampled_idle = self.received_bytes == 0;
        self.received_bytes = 0;
        speed
    }
}

#[cfg(test)]
mod tests {
    use super::{DownloadSpeedSampler, SAMPLE_INTERVAL};
    use std::time::{Duration, Instant};

    #[test]
    fn speed_sample_uses_received_blocks_and_decays_to_zero_when_idle() {
        let start = Instant::now();
        let mut sampler = DownloadSpeedSampler::at(start);
        sampler.record(1024);

        assert_eq!(sampler.sample(start + SAMPLE_INTERVAL), 2048);
        assert_eq!(
            sampler.sample(start + SAMPLE_INTERVAL + Duration::from_secs(1)),
            0
        );
        assert_eq!(sampler.next_deadline(), None);

        sampler.record(512);
        assert_eq!(
            sampler.next_deadline(),
            Some(start + SAMPLE_INTERVAL + Duration::from_secs(1) + SAMPLE_INTERVAL)
        );
    }
}
