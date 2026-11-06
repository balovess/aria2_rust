//! Dynamic segment size calculation for HTTP downloads.
//!
//! Adjusts segment sizes based on download speed and remaining data to
//! balance parallelism with overhead.

/// Calculate optimal segment size based on download speed and remaining data.
/// Returns size in bytes (between MIN_SEGMENT_SIZE and MAX_SEGMENT_SIZE).
pub fn calculate_dynamic_segment_size(
    total_remaining: u64,
    num_connections: usize,
    avg_speed_bps: f64,
    elapsed_secs: u64,
) -> u64 {
    const MIN_SEGMENT: u64 = 1024 * 256; // 256 KB
    const MAX_SEGMENT: u64 = 1024 * 1024 * 16; // 16 MB

    if elapsed_secs < 2 || avg_speed_bps < 1024.0 {
        // Too early or too slow — use conservative default
        return (total_remaining / num_connections.max(1) as u64).clamp(MIN_SEGMENT, MAX_SEGMENT);
    }

    // Target ~10 seconds per segment at current speed
    let target_size = (avg_speed_bps * 10.0) as u64;
    target_size.clamp(MIN_SEGMENT, MAX_SEGMENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dynamic_segment_size_slow_start() {
        // Early download (elapsed < 2 seconds) should use conservative default
        let size = calculate_dynamic_segment_size(10_000_000, 4, 50000.0, 1);
        // With 10MB remaining and 4 connections: 10_000_000 / 4 = 2.5MB = 2621440 bytes
        // Should be clamped between MIN_SEGMENT (256KB) and MAX_SEGMENT (16MB)
        assert!(size >= 1024 * 256, "Should be at least MIN_SEGMENT");
        assert!(size <= 1024 * 1024 * 16, "Should be at most MAX_SEGMENT");

        // Very slow speed (< 1KB/s) should also use conservative default
        let size_slow = calculate_dynamic_segment_size(10_000_000, 4, 100.0, 5);
        assert!(
            size_slow >= 1024 * 256,
            "Slow speed should use conservative default"
        );
    }

    #[test]
    fn test_dynamic_segment_size_fast_download() {
        // Fast download (1 MB/s = 1048576 B/s) with sufficient elapsed time
        let size = calculate_dynamic_segment_size(100_000_000, 8, 1_048_576.0, 10);
        // Target size = 1048576.0 * 10.0 = 10485760 bytes (~10 MB)
        // Should be clamped to MAX_SEGMENT if needed
        assert_eq!(
            size, 10_485_760,
            "Fast download should produce large segments"
        );

        // Very fast download (10 MB/s)
        let size_very_fast = calculate_dynamic_segment_size(1_000_000_000, 16, 10_485_760.0, 30);
        // Target = 104857600 bytes (~100 MB), but capped at MAX_SEGMENT (16 MB)
        assert_eq!(
            size_very_fast, 16_777_216,
            "Very fast download should be capped at MAX_SEGMENT"
        );
    }
}
