/// Per-download tuning for adaptive HTTP Range concurrency.
///
/// The option registry exposes these settings through CLI, configuration
/// files, RPC, and the shared `DownloadOptions` conversion path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpAdaptiveOptions {
    pub enabled: bool,
    pub reduction_percent: u8,
    pub recovery_interval_secs: u64,
    pub recovery_successes_min: u32,
    pub recovery_successes_max: u32,
    pub start_interval_ms: u64,
    pub backoff_base_secs: u64,
    pub backoff_max_secs: u64,
    pub capacity_start_interval_ms: u64,
    pub capacity_start_interval_max_ms: u64,
    pub rate_recovery_successes: u32,
    pub rate_min_interval_ms: u64,
    pub backoff_reset_after_secs: u64,
}

impl HttpAdaptiveOptions {
    pub(crate) fn from_option_strings(options: &std::collections::HashMap<String, String>) -> Self {
        let parse = |name: &str, default: u64| {
            options
                .get(name)
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(default)
        };

        Self {
            enabled: options
                .get("http-adaptive-enabled")
                .and_then(|value| value.parse::<bool>().ok())
                .unwrap_or(false),
            reduction_percent: parse("http-adaptive-reduction-percent", 25).clamp(1, 90) as u8,
            recovery_interval_secs: parse("http-adaptive-recovery-interval", 10).clamp(1, 3600),
            recovery_successes_min: parse("http-adaptive-recovery-successes-min", 8)
                .clamp(1, 100_000) as u32,
            recovery_successes_max: parse("http-adaptive-recovery-successes-max", 32)
                .clamp(1, 100_000) as u32,
            start_interval_ms: parse("http-adaptive-start-interval-ms", 100).min(60_000),
            backoff_base_secs: parse("http-adaptive-backoff-base", 20).clamp(1, 3600),
            backoff_max_secs: parse("http-adaptive-backoff-max", 300).clamp(1, 86_400),
            capacity_start_interval_ms: parse("http-adaptive-capacity-start-interval-ms", 1000)
                .clamp(1, 60_000),
            capacity_start_interval_max_ms: parse(
                "http-adaptive-capacity-start-interval-max-ms",
                4000,
            )
            .clamp(1, 60_000),
            rate_recovery_successes: parse("http-adaptive-rate-recovery-successes", 64)
                .clamp(1, 1_000_000) as u32,
            rate_min_interval_ms: parse("http-adaptive-rate-min-interval-ms", 10).min(60_000),
            backoff_reset_after_secs: parse("http-adaptive-backoff-reset-after", 60)
                .clamp(1, 86_400),
        }
        .normalized()
    }

    pub(crate) fn normalized(mut self) -> Self {
        self.reduction_percent = self.reduction_percent.clamp(1, 90);
        self.recovery_interval_secs = self.recovery_interval_secs.clamp(1, 3600);
        self.recovery_successes_min = self.recovery_successes_min.clamp(1, 100_000);
        self.recovery_successes_max = self.recovery_successes_max.clamp(1, 100_000);
        self.start_interval_ms = self.start_interval_ms.min(60_000);
        self.backoff_base_secs = self.backoff_base_secs.clamp(1, 3600);
        self.backoff_max_secs = self.backoff_max_secs.clamp(1, 86_400);
        self.capacity_start_interval_ms = self.capacity_start_interval_ms.clamp(1, 60_000);
        self.capacity_start_interval_max_ms = self.capacity_start_interval_max_ms.clamp(1, 60_000);
        self.rate_recovery_successes = self.rate_recovery_successes.clamp(1, 1_000_000);
        self.rate_min_interval_ms = self.rate_min_interval_ms.min(60_000);
        self.backoff_reset_after_secs = self.backoff_reset_after_secs.clamp(1, 86_400);
        self.recovery_successes_max = self.recovery_successes_max.max(self.recovery_successes_min);
        self.backoff_max_secs = self.backoff_max_secs.max(self.backoff_base_secs);
        self.capacity_start_interval_max_ms = self
            .capacity_start_interval_max_ms
            .max(self.capacity_start_interval_ms);
        self
    }
}

impl Default for HttpAdaptiveOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            reduction_percent: 25,
            recovery_interval_secs: 10,
            recovery_successes_min: 8,
            recovery_successes_max: 32,
            start_interval_ms: 100,
            backoff_base_secs: 20,
            backoff_max_secs: 300,
            capacity_start_interval_ms: 1000,
            capacity_start_interval_max_ms: 4000,
            rate_recovery_successes: 64,
            rate_min_interval_ms: 10,
            backoff_reset_after_secs: 60,
        }
    }
}
