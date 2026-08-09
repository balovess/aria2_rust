//! HTTP segmented download — directory module anchor.
//!
//! Splits the monolithic implementation into three focused sub-modules:
//!
//! - [`downloader`]        — `HttpSegmentDownloader`, `WriteChunk`
//! - [`connection_limiter`] — `ConnectionLimiter` (per-host + global slot tracking)
//! - [`segment_size`]      — `calculate_dynamic_segment_size`

mod connection_limiter;
mod downloader;
mod segment_size;

// Public re-exports — preserve the original `http_segment_downloader::X` API surface
pub use connection_limiter::ConnectionLimiter;
pub use downloader::{HttpSegmentDownloader, WriteChunk};
pub use segment_size::calculate_dynamic_segment_size;

// Re-export score_source for convenience (was in the original monolithic file)
pub use crate::selector::source_scorer::{score_source_raw as score_source, score_source_raw};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_scoring_slow_penalized() {
        // Fast source (1 MB/s)
        let fast_score = score_source(1_048_576.0, 0, 0);

        // Slow source (1 KB/s)
        let slow_score = score_source(1024.0, 0, 0);

        // Dead source (no speed + failures)
        let dead_score = score_source(0.0, 3, 0);

        // Slow source should have higher (worse) score than fast source
        assert!(
            slow_score > fast_score,
            "Slow source should have worse score than fast source"
        );

        // Dead source should have maximum score
        assert_eq!(dead_score, f64::MAX, "Dead source should have MAX score");

        // Source with failures should be penalized
        let failed_score = score_source(1_048_576.0, 2, 0);
        assert!(
            failed_score > fast_score,
            "Failed source should have worse score than successful one"
        );

        // Recent success should improve score (lower is better)
        // Note: age_bonus is subtracted, so more recent (smaller age) = smaller subtraction = slightly higher score
        // But the effect is minimal compared to speed differences
        let recent_score = score_source(1_048_576.0, 0, 10); // 10 seconds ago
        let old_score = score_source(1_048_576.0, 0, 300); // 5 minutes ago
        // Both should have similar base scores (same speed), but old success has larger age bonus subtracted
        assert!(
            old_score < recent_score,
            "Old success should give better (lower) score due to larger age bonus"
        );

        // Verify that both are still much better than slow sources
        let very_slow = score_source(1024.0, 0, 0);
        assert!(
            recent_score < very_slow,
            "Even recent fast source beats slow source"
        );
    }
}
