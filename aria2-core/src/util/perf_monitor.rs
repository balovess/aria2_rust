//! Performance monitoring module for aria2-rust
//!
//! This module provides lightweight performance metrics collection with minimal overhead (< 1%).
//! It tracks throughput, latency, memory usage, and lock wait times.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Performance metrics snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metrics {
    /// Throughput in bytes per second
    pub throughput: u64,
    /// Latency in milliseconds
    pub latency: u64,
    /// Memory usage in bytes
    pub memory_usage: u64,
    /// Lock wait time in milliseconds
    pub lock_wait_time: u64,
    /// Timestamp when metrics were recorded (epoch millis)
    pub timestamp: u64,
    /// Optional label for the metric
    pub label: Option<String>,
}

impl Metrics {
    /// Create a new metrics snapshot
    pub fn new(throughput: u64, latency: u64, memory_usage: u64, lock_wait_time: u64) -> Self {
        Self {
            throughput,
            latency,
            memory_usage,
            lock_wait_time,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            label: None,
        }
    }

    /// Create metrics with a label
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Calculate the overall performance score (0-100)
    pub fn performance_score(&self) -> f64 {
        // Simple scoring: higher throughput and lower latency/lock_wait is better
        let throughput_score = (self.throughput as f64 / 1_000_000.0).min(50.0); // Max 50 points for throughput
        let latency_penalty = (self.latency as f64 / 100.0).min(25.0); // Max 25 points penalty
        let lock_penalty = (self.lock_wait_time as f64 / 100.0).min(25.0); // Max 25 points penalty

        (throughput_score + 50.0 - latency_penalty - lock_penalty).clamp(0.0, 100.0)
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new(0, 0, 0, 0)
    }
}

/// Atomic counters for low-overhead metric collection
#[derive(Debug)]
pub struct AtomicMetrics {
    throughput: AtomicU64,
    latency: AtomicU64,
    memory_usage: AtomicU64,
    lock_wait_time: AtomicU64,
    start_time: Instant,
}

impl AtomicMetrics {
    /// Create new atomic metrics
    pub fn new() -> Self {
        Self {
            throughput: AtomicU64::new(0),
            latency: AtomicU64::new(0),
            memory_usage: AtomicU64::new(0),
            lock_wait_time: AtomicU64::new(0),
            start_time: Instant::now(),
        }
    }

    /// Record throughput (bytes/sec)
    #[inline]
    pub fn record_throughput(&self, bytes_per_sec: u64) {
        self.throughput.store(bytes_per_sec, Ordering::Relaxed);
    }

    /// Record latency (ms)
    #[inline]
    pub fn record_latency(&self, ms: u64) {
        self.latency.store(ms, Ordering::Relaxed);
    }

    /// Record memory usage (bytes)
    #[inline]
    pub fn record_memory(&self, bytes: u64) {
        self.memory_usage.store(bytes, Ordering::Relaxed);
    }

    /// Record lock wait time (ms)
    #[inline]
    pub fn record_lock_wait(&self, ms: u64) {
        self.lock_wait_time.fetch_add(ms, Ordering::Relaxed);
    }

    /// Snapshot current metrics
    pub fn snapshot(&self) -> Metrics {
        Metrics::new(
            self.throughput.load(Ordering::Relaxed),
            self.latency.load(Ordering::Relaxed),
            self.memory_usage.load(Ordering::Relaxed),
            self.lock_wait_time.load(Ordering::Relaxed),
        )
    }

    /// Get elapsed time since monitoring started
    pub fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }

    /// Reset all counters
    pub fn reset(&self) {
        self.throughput.store(0, Ordering::Relaxed);
        self.latency.store(0, Ordering::Relaxed);
        self.memory_usage.store(0, Ordering::Relaxed);
        self.lock_wait_time.store(0, Ordering::Relaxed);
    }
}

impl Default for AtomicMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Performance report containing aggregated metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceReport {
    /// Collection of metrics by label
    pub metrics: HashMap<String, Vec<Metrics>>,
    /// Report generation timestamp
    pub generated_at: u64,
    /// Total duration of monitoring (ms)
    pub duration_ms: u64,
    /// Summary statistics
    pub summary: ReportSummary,
}

impl PerformanceReport {
    /// Create a new performance report
    pub fn new(metrics: HashMap<String, Vec<Metrics>>, duration_ms: u64) -> Self {
        let summary = Self::calculate_summary(&metrics);
        Self {
            metrics,
            generated_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            duration_ms,
            summary,
        }
    }

    /// Calculate summary statistics from metrics
    fn calculate_summary(metrics: &HashMap<String, Vec<Metrics>>) -> ReportSummary {
        let mut total_throughput = 0u64;
        let mut total_latency = 0u64;
        let mut total_memory = 0u64;
        let mut total_lock_wait = 0u64;
        let mut count = 0usize;

        for metric_list in metrics.values() {
            for m in metric_list.iter() {
                total_throughput += m.throughput;
                total_latency += m.latency;
                total_memory += m.memory_usage;
                total_lock_wait += m.lock_wait_time;
                count += 1;
            }
        }

        ReportSummary {
            avg_throughput: if count > 0 {
                total_throughput / count as u64
            } else {
                0
            },
            avg_latency: if count > 0 {
                total_latency / count as u64
            } else {
                0
            },
            avg_memory_usage: if count > 0 {
                total_memory / count as u64
            } else {
                0
            },
            avg_lock_wait_time: if count > 0 {
                total_lock_wait / count as u64
            } else {
                0
            },
            total_samples: count,
        }
    }
}

/// Summary statistics for the performance report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportSummary {
    /// Average throughput (bytes/sec)
    pub avg_throughput: u64,
    /// Average latency (ms)
    pub avg_latency: u64,
    /// Average memory usage (bytes)
    pub avg_memory_usage: u64,
    /// Average lock wait time (ms)
    pub avg_lock_wait_time: u64,
    /// Total number of samples
    pub total_samples: usize,
}

/// Performance monitor for collecting and reporting metrics
pub struct PerformanceMonitor {
    metrics: Arc<tokio::sync::RwLock<HashMap<String, Vec<Metrics>>>>,
    start_time: Instant,
}

impl PerformanceMonitor {
    /// Create a new performance monitor
    pub fn new() -> Self {
        Self {
            metrics: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            start_time: Instant::now(),
        }
    }

    /// Get the elapsed time since monitoring started
    pub fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }
}

impl Default for PerformanceMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl PerformanceMonitor {
    /// Record a metric with the given label
    pub fn record_metric(&self, label: &str, metrics: Metrics) {
        // Use try_write to avoid blocking in hot paths
        // This is a trade-off: we might miss some metrics under high contention
        // but it ensures minimal overhead
        if let Ok(mut guard) = self.metrics.try_write() {
            guard
                .entry(label.to_string())
                .or_insert_with(Vec::new)
                .push(metrics);
        }
    }

    /// Generate a performance report
    pub fn generate_report(&self) -> PerformanceReport {
        // Use try_read to avoid blocking in async contexts
        // If we can't get the lock, return an empty report
        let metrics = self
            .metrics
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| HashMap::new());
        PerformanceReport::new(metrics, self.elapsed().as_millis() as u64)
    }

    /// Export metrics as JSON string
    pub fn export_json(&self) -> String {
        let report = self.generate_report();
        serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_string())
    }

    /// Export metrics as human-readable text
    pub fn export_text(&self) -> String {
        let report = self.generate_report();
        let mut output = String::new();

        output.push_str("Performance Report\n");
        output.push_str("==================\n");
        output.push_str(&format!("Generated at: {} ms\n", report.generated_at));
        output.push_str(&format!("Duration: {} ms\n\n", report.duration_ms));

        output.push_str("Summary:\n");
        output.push_str("--------\n");
        output.push_str(&format!(
            "  Total samples: {}\n",
            report.summary.total_samples
        ));
        output.push_str(&format!(
            "  Avg throughput: {} bytes/sec\n",
            report.summary.avg_throughput
        ));
        output.push_str(&format!(
            "  Avg latency: {} ms\n",
            report.summary.avg_latency
        ));
        output.push_str(&format!(
            "  Avg memory usage: {} bytes\n",
            report.summary.avg_memory_usage
        ));
        output.push_str(&format!(
            "  Avg lock wait time: {} ms\n\n",
            report.summary.avg_lock_wait_time
        ));

        output.push_str("Detailed Metrics:\n");
        output.push_str("-----------------\n");
        for (label, metrics_list) in &report.metrics {
            output.push_str(&format!("\n[{}]\n", label));
            for (i, m) in metrics_list.iter().enumerate() {
                output.push_str(&format!(
                    "  Sample {}: throughput={} B/s, latency={} ms, memory={} B, lock_wait={} ms\n",
                    i + 1,
                    m.throughput,
                    m.latency,
                    m.memory_usage,
                    m.lock_wait_time
                ));
            }
        }

        output
    }
}

/// RAII guard for measuring operation duration
pub struct ScopedTimer {
    label: String,
    start: Instant,
    monitor: Arc<PerformanceMonitor>,
}

impl ScopedTimer {
    /// Create a new scoped timer
    pub fn new(label: impl Into<String>, monitor: Arc<PerformanceMonitor>) -> Self {
        Self {
            label: label.into(),
            start: Instant::now(),
            monitor,
        }
    }
}

impl Drop for ScopedTimer {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed().as_millis() as u64;
        let metrics = Metrics::new(0, elapsed, 0, 0).with_label(&self.label);
        self.monitor.record_metric(&self.label, metrics);
    }
}

/// Helper macro for creating a scoped timer
#[macro_export]
macro_rules! scoped_perf_timer {
    ($label:expr, $monitor:expr) => {
        $crate::util::perf_monitor::ScopedTimer::new($label, $monitor)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_creation() {
        let m = Metrics::new(1000, 50, 1024 * 1024, 10);
        assert_eq!(m.throughput, 1000);
        assert_eq!(m.latency, 50);
        assert_eq!(m.memory_usage, 1024 * 1024);
        assert_eq!(m.lock_wait_time, 10);
        assert!(m.timestamp > 0);
    }

    #[test]
    fn test_metrics_with_label() {
        let m = Metrics::new(1000, 50, 1024, 10).with_label("download");
        assert_eq!(m.label, Some("download".to_string()));
    }

    #[test]
    fn test_performance_score() {
        // High throughput, low latency should have high score
        let m1 = Metrics::new(10_000_000, 10, 1024, 5);
        let score1 = m1.performance_score();
        assert!(score1 > 50.0, "Score should be > 50, got {}", score1);

        // Low throughput, high latency should have lower score
        let m2 = Metrics::new(1000, 1000, 1024, 100);
        let score2 = m2.performance_score();
        assert!(score2 < score1, "Score should be lower, got {}", score2);
    }

    #[test]
    fn test_atomic_metrics() {
        let am = AtomicMetrics::new();
        am.record_throughput(1000);
        am.record_latency(50);
        am.record_memory(1024);
        am.record_lock_wait(10);

        let snapshot = am.snapshot();
        assert_eq!(snapshot.throughput, 1000);
        assert_eq!(snapshot.latency, 50);
        assert_eq!(snapshot.memory_usage, 1024);
        assert_eq!(snapshot.lock_wait_time, 10);
    }

    #[test]
    fn test_atomic_metrics_reset() {
        let am = AtomicMetrics::new();
        am.record_throughput(1000);
        am.record_latency(50);
        am.reset();

        let snapshot = am.snapshot();
        assert_eq!(snapshot.throughput, 0);
        assert_eq!(snapshot.latency, 0);
    }

    #[test]
    fn test_performance_monitor() {
        let monitor = PerformanceMonitor::new();
        let m1 = Metrics::new(1000, 50, 1024, 10);
        let m2 = Metrics::new(2000, 30, 2048, 5);

        monitor.record_metric("download", m1);
        monitor.record_metric("download", m2);

        let report = monitor.generate_report();
        assert!(report.metrics.contains_key("download"));
        assert_eq!(report.metrics.get("download").unwrap().len(), 2);
        assert_eq!(report.summary.total_samples, 2);
        assert_eq!(report.summary.avg_throughput, 1500); // (1000 + 2000) / 2
    }

    #[test]
    fn test_export_json() {
        let monitor = PerformanceMonitor::new();
        let m = Metrics::new(1000, 50, 1024, 10);
        monitor.record_metric("test", m);

        let json = monitor.export_json();
        assert!(json.contains("test"));
        assert!(json.contains("throughput"));
    }

    #[test]
    fn test_export_text() {
        let monitor = PerformanceMonitor::new();
        let m = Metrics::new(1000, 50, 1024, 10);
        monitor.record_metric("test", m);

        let text = monitor.export_text();
        assert!(text.contains("Performance Report"));
        assert!(text.contains("test"));
        assert!(text.contains("1000 B/s"));
    }

    #[test]
    fn test_scoped_timer() {
        let monitor = Arc::new(PerformanceMonitor::new());
        {
            let _timer = ScopedTimer::new("operation", monitor.clone());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let report = monitor.generate_report();
        assert!(report.metrics.contains_key("operation"));
        let metrics = report.metrics.get("operation").unwrap();
        assert!(!metrics.is_empty());
        assert!(metrics[0].latency >= 10);
    }

    #[tokio::test]
    async fn test_concurrent_recording() {
        let monitor = Arc::new(PerformanceMonitor::new());
        let mut handles = vec![];

        for i in 0..10 {
            let m = monitor.clone();
            handles.push(tokio::spawn(async move {
                let metric = Metrics::new(i * 100, i * 10, i * 1024, i);
                m.record_metric("concurrent", metric);
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        let report = monitor.generate_report();
        assert!(report.metrics.contains_key("concurrent"));
        // Note: Due to try_write, some metrics might be missed under high contention
        // This is acceptable for minimal overhead
    }
}
