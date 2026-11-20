use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, Weak};

use super::WrDiskCache;

type RegisteredCache = (u64, Weak<WrDiskCache>, usize);

/// Keep enough write-back cache per active task to avoid flushing on every
/// network chunk under high concurrency. The aggregate remains bounded by
/// `max(global_limit, active_caches * MIN_CACHE_SHARE_BYTES)`.
const MIN_CACHE_SHARE_BYTES: usize = 4 * 1024 * 1024;

/// Process-wide write-back cache budget.
///
/// Each cache keeps ownership of its underlying writer. The budget only
/// recomputes a per-cache quota from the largest requested cache size and the
/// number of live caches, so the aggregate dirty-cache memory stays bounded.
pub(crate) struct DiskCacheBudget {
    next_id: AtomicU64,
    caches: Mutex<Vec<RegisteredCache>>,
}

static DISK_CACHE_BUDGET: LazyLock<DiskCacheBudget> = LazyLock::new(|| DiskCacheBudget {
    next_id: AtomicU64::new(1),
    caches: Mutex::new(Vec::new()),
});

pub(crate) fn disk_cache_budget() -> &'static DiskCacheBudget {
    &DISK_CACHE_BUDGET
}

impl DiskCacheBudget {
    pub(crate) fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn register(&self, id: u64, cache: &Arc<WrDiskCache>, requested: usize) {
        {
            let mut caches = self
                .caches
                .lock()
                .expect("disk cache budget lock is poisoned");
            caches.retain(|(_, weak, _)| weak.strong_count() > 0);
            caches.push((id, Arc::downgrade(cache), requested));
        }
        self.recompute();
    }

    pub(crate) fn unregister(&self, id: u64) {
        {
            let mut caches = self
                .caches
                .lock()
                .expect("disk cache budget lock is poisoned");
            caches
                .retain(|(registered_id, weak, _)| *registered_id != id && weak.strong_count() > 0);
        }
        self.recompute();
    }

    fn recompute(&self) {
        let caches = self
            .caches
            .lock()
            .expect("disk cache budget lock is poisoned");
        let Some(global_limit) = caches.iter().map(|(_, _, requested)| *requested).max() else {
            return;
        };
        let active = caches.len().max(1);
        let fair_share = global_limit.div_ceil(active);
        for (_, weak, requested) in caches.iter() {
            if let Some(cache) = weak.upgrade() {
                let floor = MIN_CACHE_SHARE_BYTES.min(*requested);
                cache.set_budget_limit((*requested).min(fair_share.max(floor)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_caches_share_the_largest_requested_budget() {
        let mib = 1024 * 1024;
        let first = Arc::new(WrDiskCache::with_max_size_bytes(20 * mib));
        let second = Arc::new(WrDiskCache::with_max_size_bytes(12 * mib));
        WrDiskCache::register(&first);
        WrDiskCache::register(&second);

        assert_eq!(first.max_size_bytes(), 10 * mib);
        assert_eq!(second.max_size_bytes(), 10 * mib);

        drop(second);
        assert_eq!(first.max_size_bytes(), 20 * mib);

        drop(first);
    }
}
