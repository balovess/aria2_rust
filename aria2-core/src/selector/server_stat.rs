use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const EMA_ALPHA: f64 = 0.7;

#[derive(Debug)]
pub struct ServerStat {
    pub host: String,
    download_speed: AtomicU64,
    single_connection_avg_speed: AtomicU64,
    multi_connection_avg_speed: AtomicU64,
    last_updated: AtomicU64,
    status: AtomicU8,
    counter: AtomicU32,
}

impl Clone for ServerStat {
    fn clone(&self) -> Self {
        Self {
            host: self.host.clone(),
            download_speed: AtomicU64::new(self.download_speed.load(Ordering::Relaxed)),
            single_connection_avg_speed: AtomicU64::new(
                self.single_connection_avg_speed.load(Ordering::Relaxed),
            ),
            multi_connection_avg_speed: AtomicU64::new(
                self.multi_connection_avg_speed.load(Ordering::Relaxed),
            ),
            last_updated: AtomicU64::new(self.last_updated.load(Ordering::Relaxed)),
            status: AtomicU8::new(self.status.load(Ordering::Relaxed)),
            counter: AtomicU32::new(self.counter.load(Ordering::Relaxed)),
        }
    }
}

impl ServerStat {
    pub fn new(host: &str) -> Self {
        Self {
            host: host.to_string(),
            download_speed: AtomicU64::new(0),
            single_connection_avg_speed: AtomicU64::new(0),
            multi_connection_avg_speed: AtomicU64::new(0),
            last_updated: AtomicU64::new(0),
            status: AtomicU8::new(0),
            counter: AtomicU32::new(0),
        }
    }

    pub fn update_speed(&self, speed: u64, is_multi: bool) {
        self.download_speed.store(speed, Ordering::Relaxed);
        if is_multi {
            let old = self.multi_connection_avg_speed.load(Ordering::Relaxed);
            let new = ema(old, speed);
            self.multi_connection_avg_speed
                .store(new, Ordering::Relaxed);
        } else {
            let old = self.single_connection_avg_speed.load(Ordering::Relaxed);
            let new = ema(old, speed);
            self.single_connection_avg_speed
                .store(new, Ordering::Relaxed);
        }
        self.touch();
    }

    pub fn get_download_speed(&self) -> u64 {
        self.download_speed.load(Ordering::Relaxed)
    }

    pub fn get_single_avg_speed(&self) -> u64 {
        self.single_connection_avg_speed.load(Ordering::Relaxed)
    }

    pub fn get_multi_avg_speed(&self) -> u64 {
        self.multi_connection_avg_speed.load(Ordering::Relaxed)
    }

    pub fn get_avg_speed(&self) -> u64 {
        let s = self.single_connection_avg_speed.load(Ordering::Relaxed);
        let m = self.multi_connection_avg_speed.load(Ordering::Relaxed);
        if s > 0 && m > 0 {
            (s + m) / 2
        } else {
            s.max(m)
        }
    }

    pub fn is_ok(&self) -> bool {
        self.status.load(Ordering::Relaxed) == 0
    }

    pub fn set_error(&self) {
        self.status.store(1, Ordering::Relaxed);
    }

    pub fn reset_status(&self) {
        self.status.store(0, Ordering::Relaxed);
    }

    pub fn increment_counter(&self) -> u32 {
        self.counter.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    }

    pub fn get_counter(&self) -> u32 {
        self.counter.load(Ordering::Relaxed)
    }

    pub fn reset_counter(&self) {
        self.counter.store(0, Ordering::Relaxed);
    }

    pub fn is_fresh(&self, duration_secs: u64) -> bool {
        let last = self.last_updated.load(Ordering::Relaxed);
        if last == 0 {
            return false;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now.saturating_sub(last) < duration_secs
    }

    fn touch(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.last_updated.store(now, Ordering::Relaxed);
    }
}

fn ema(old: u64, new: u64) -> u64 {
    (old as f64 * (1.0 - EMA_ALPHA) + new as f64 * EMA_ALPHA) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_creation() {
        let stat = ServerStat::new("example.com");
        assert_eq!(stat.host, "example.com");
        assert_eq!(stat.get_download_speed(), 0);
        assert_eq!(stat.get_single_avg_speed(), 0);
        assert!(stat.is_ok());
        assert_eq!(stat.get_counter(), 0);
    }

    #[test]
    fn test_update_single_speed() {
        let stat = ServerStat::new("example.com");
        stat.update_speed(1000, false);
        assert_eq!(stat.get_download_speed(), 1000);
        assert_eq!(stat.get_single_avg_speed(), 700); // 0*0.3 + 1000*0.7

        stat.update_speed(2000, false);
        assert_eq!(stat.get_single_avg_speed(), 1610); // 700*0.3 + 2000*0.7
    }

    #[test]
    fn test_update_multi_speed_independent() {
        let stat = ServerStat::new("example.com");
        stat.update_speed(1000, true);
        assert_eq!(stat.get_multi_avg_speed(), 700);
        assert_eq!(stat.get_single_avg_speed(), 0);

        stat.update_speed(500, false);
        assert_eq!(stat.get_single_avg_speed(), 350);
        assert_eq!(stat.get_multi_avg_speed(), 700);
    }

    #[test]
    fn test_get_avg_speed_combines_both() {
        let stat = ServerStat::new("example.com");
        stat.update_speed(1000, false);
        stat.update_speed(2000, true);
        let avg = stat.get_avg_speed();
        assert!(avg > 0);
        assert!(avg >= 350 && avg <= 1400);
    }

    #[test]
    fn test_status_toggle() {
        let stat = ServerStat::new("example.com");
        assert!(stat.is_ok());

        stat.set_error();
        assert!(!stat.is_ok());

        stat.reset_status();
        assert!(stat.is_ok());
    }

    #[test]
    fn test_counter_operations() {
        let stat = ServerStat::new("example.com");
        assert_eq!(stat.get_counter(), 0);

        let c1 = stat.increment_counter();
        assert_eq!(c1, 1);
        assert_eq!(stat.get_counter(), 1);

        let c2 = stat.increment_counter();
        assert_eq!(c2, 2);

        stat.reset_counter();
        assert_eq!(stat.get_counter(), 0);
    }

    #[test]
    fn test_is_fresh_after_update() {
        let stat = ServerStat::new("example.com");
        assert!(!stat.is_fresh(60));

        stat.update_speed(1000, false);
        assert!(stat.is_fresh(60));
        assert!(!stat.is_fresh(0));
    }

    #[test]
    fn test_concurrent_updates() {
        use std::sync::Arc;
        use std::thread;

        let stat = Arc::new(ServerStat::new("concurrent.test"));
        let mut handles = Vec::new();

        for i in 0..10u64 {
            let s = Arc::clone(&stat);
            handles.push(thread::spawn(move || {
                s.update_speed((i + 1) * 1000, i % 2 == 0);
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert!(stat.get_download_speed() > 0);
        assert!(stat.is_fresh(60));
    }
}
