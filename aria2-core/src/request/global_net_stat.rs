//! Lock-free process-session transfer counters shared by request groups.

use std::sync::atomic::{AtomicU64, Ordering};

/// Aggregate transfer counters for one `RequestGroupMan` session.
///
/// These counters are separate from each download's `NetStat`, so queue
/// movement does not affect their lifetime and hot-path updates avoid the
/// manager's async lock.
#[derive(Debug, Default)]
pub(crate) struct GlobalNetStat {
    session_download_length: AtomicU64,
    session_upload_length: AtomicU64,
    upload_speed_sample: AtomicU64,
}

impl GlobalNetStat {
    pub(crate) fn update_download(&self, bytes: u64) {
        saturating_add(&self.session_download_length, bytes);
    }

    pub(crate) fn update_upload_length(&self, bytes: u64) {
        saturating_add(&self.session_upload_length, bytes);
    }

    /// Keep the latest sample supplied by a protocol implementation.
    pub(crate) fn update_upload_speed(&self, bytes: u64) {
        self.upload_speed_sample.store(bytes, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn session_download_length_for_test(&self) -> u64 {
        self.session_download_length.load(Ordering::Relaxed)
    }
}

fn saturating_add(counter: &AtomicU64, amount: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(amount))
    });
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::GlobalNetStat;

    #[test]
    fn counters_saturate_and_keep_the_latest_upload_sample() {
        let stats = GlobalNetStat::default();

        stats.update_download(u64::MAX);
        stats.update_download(1);
        stats.update_upload_length(u64::MAX);
        stats.update_upload_length(1);
        stats.update_upload_speed(4096);

        assert_eq!(stats.session_download_length_for_test(), u64::MAX);
        assert_eq!(
            stats.session_upload_length.load(Ordering::Relaxed),
            u64::MAX
        );
        assert_eq!(stats.upload_speed_sample.load(Ordering::Relaxed), 4096);
    }
}
