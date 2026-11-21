/// Per-download tuning for adaptive HTTP Range concurrency.
///
/// The option registry exposes these settings through CLI, configuration
/// files, RPC, and the shared `DownloadOptions` conversion path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpAdaptiveOptions {
    pub enabled: bool,
    pub reduction_percent: u8,
    pub backoff_base_secs: u64,
    pub backoff_max_secs: u64,
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
                .unwrap_or(true),
            reduction_percent: parse("http-adaptive-reduction-percent", 50).clamp(1, 90) as u8,
            backoff_base_secs: parse("http-adaptive-backoff-base", 20).clamp(1, 3600),
            backoff_max_secs: parse("http-adaptive-backoff-max", 300).clamp(1, 86_400),
        }
        .normalized()
    }

    pub(crate) fn normalized(mut self) -> Self {
        self.reduction_percent = self.reduction_percent.clamp(1, 90);
        self.backoff_base_secs = self.backoff_base_secs.clamp(1, 3600);
        self.backoff_max_secs = self.backoff_max_secs.clamp(1, 86_400);
        self.backoff_max_secs = self.backoff_max_secs.max(self.backoff_base_secs);
        self
    }
}

impl Default for HttpAdaptiveOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            reduction_percent: 50,
            backoff_base_secs: 20,
            backoff_max_secs: 300,
        }
    }
}
