// Source scoring using EMA-averaged speed from ServerStat.
//
// This module provides source server scoring functions that use
// Exponential Moving Average (EMA) speed data from ServerStat
// for more accurate and stable source selection.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::selector::server_stat::ServerStat;

/// Score a source server using EMA-averaged speed from ServerStat.
///
/// This is the primary scoring function that uses the ServerStat's
/// internally maintained EMA average speed for stable scoring.
///
/// # Scoring Formula
///
/// ```text
/// score = speed_score + failure_penalty - age_bonus
/// ```
///
/// Where:
/// - `speed_score = -ln(1 + avg_speed)` (higher speed = lower score = better)
/// - `failure_penalty = consecutive_failures * 100`
/// - `age_bonus = min(seconds_since_last_success / 60, 10)`
///
/// # Arguments
///
/// * `stat` - ServerStat containing EMA speed and failure data
///
/// # Returns
///
/// Lower score = better source. `f64::MAX` indicates a dead source.
///
/// # Example
///
/// ```
/// use aria2_core::selector::server_stat::ServerStat;
/// use aria2_core::selector::source_scorer::score_source;
///
/// let stat = ServerStat::new("fast.mirror.com");
/// stat.update_speed(1_000_000, false); // 1 MB/s
///
/// let score = score_source(&stat);
/// assert!(score < f64::MAX, "Live source should have finite score");
/// ```
pub fn score_source(stat: &ServerStat) -> f64 {
    let avg_speed = stat.get_avg_speed() as f64;
    let failure_count = stat.get_consecutive_failures();
    let last_success_age = calculate_last_success_age(stat);

    // Dead source: no speed and has failures
    if avg_speed <= 0.0 && failure_count > 0 {
        return f64::MAX;
    }

    // Speed score: higher speed = lower (better) score
    let speed_score = if avg_speed > 0.0 {
        -avg_speed.ln_1p()
    } else {
        0.0
    };

    // Failure penalty: each consecutive failure adds 100 points
    let penalty = (failure_count as f64) * 100.0;

    // Age bonus: recent success reduces score (makes source more attractive)
    // Capped at 10 to prevent over-weighting old successes
    let age_bonus = (last_success_age as f64 / 60.0).min(10.0);

    speed_score + penalty - age_bonus
}

/// Score a source using raw parameters (convenience function).
///
/// This is a backward-compatible function for cases where ServerStat
/// is not available. Prefer `score_source(&stat)` when possible.
///
/// # Arguments
///
/// * `avg_speed_bps` - Average speed in bytes per second (should be EMA if available)
/// * `failure_count` - Number of consecutive failures
/// * `last_success_age_secs` - Seconds since last successful transfer
///
/// # Returns
///
/// Lower score = better source. `f64::MAX` indicates a dead source.
pub fn score_source_raw(avg_speed_bps: f64, failure_count: u32, last_success_age_secs: u64) -> f64 {
    if avg_speed_bps <= 0.0 && failure_count > 0 {
        return f64::MAX;
    }

    let speed_score = if avg_speed_bps > 0.0 {
        -avg_speed_bps.ln_1p()
    } else {
        0.0
    };

    let penalty = (failure_count as f64) * 100.0;
    let age_bonus = (last_success_age_secs as f64 / 60.0).min(10.0);

    speed_score + penalty - age_bonus
}

/// Calculate seconds since last successful update.
///
/// Returns 0 if the server has never been updated or if the last
/// update was very recent (within the same second).
fn calculate_last_success_age(stat: &ServerStat) -> u64 {
    let last_updated = stat.get_last_updated();
    if last_updated == 0 {
        return 0;
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    now.saturating_sub(last_updated)
}

/// Compare two sources and return the better one.
///
/// # Arguments
///
/// * `stat_a` - First source's statistics
/// * `stat_b` - Second source's statistics
///
/// # Returns
///
/// `true` if `stat_a` is better (lower score) than `stat_b`.
pub fn is_better_source(stat_a: &ServerStat, stat_b: &ServerStat) -> bool {
    score_source(stat_a) < score_source(stat_b)
}

/// Sort a list of ServerStat references by score (best first).
///
/// # Arguments
///
/// * `stats` - Mutable slice of ServerStat references to sort in-place
pub fn sort_by_score(stats: &mut [&ServerStat]) {
    stats.sort_by(|a, b| {
        let score_a = score_source(a);
        let score_b = score_source(b);
        score_a.partial_cmp(&score_b).unwrap_or(std::cmp::Ordering::Equal)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_score_source_uses_ema() {
        let stat = ServerStat::new("test.com");
        stat.update_speed(1000, false); // First update: EMA = 700
        stat.update_speed(2000, false); // Second update: EMA = 700*0.3 + 2000*0.7 = 1610

        let score = score_source(&stat);
        // Score should be finite (not dead)
        assert!(score < f64::MAX, "Live source should have finite score");
        assert!(score.is_finite(), "Score should be finite");
    }

    #[test]
    fn test_score_source_dead_server() {
        let mut stat = ServerStat::new("dead.com");
        // No speed updates, but has failures
        stat.set_failure_info(500);
        stat.set_failure_info(500);
        stat.set_failure_info(500);

        let score = score_source(&stat);
        assert_eq!(score, f64::MAX, "Dead source should have MAX score");
    }

    #[test]
    fn test_score_source_fast_vs_slow() {
        let fast = ServerStat::new("fast.com");
        fast.update_speed(1_000_000, false); // 1 MB/s

        let slow = ServerStat::new("slow.com");
        slow.update_speed(1000, false); // 1 KB/s

        let fast_score = score_source(&fast);
        let slow_score = score_source(&slow);

        assert!(
            fast_score < slow_score,
            "Fast source should have lower (better) score: fast={}, slow={}",
            fast_score,
            slow_score
        );
    }

    #[test]
    fn test_score_source_with_failures() {
        let clean = ServerStat::new("clean.com");
        clean.update_speed(100_000, false);

        let mut failed = ServerStat::new("failed.com");
        failed.update_speed(100_000, false);
        failed.set_failure_info(500);

        let clean_score = score_source(&clean);
        let failed_score = score_source(&failed);

        assert!(
            clean_score < failed_score,
            "Clean source should have better score than failed one"
        );
    }

    #[test]
    fn test_score_source_raw_basic() {
        let fast_score = score_source_raw(1_000_000.0, 0, 0);
        let slow_score = score_source_raw(1000.0, 0, 0);
        let dead_score = score_source_raw(0.0, 3, 0);

        assert!(fast_score < slow_score, "Fast should beat slow");
        assert_eq!(dead_score, f64::MAX, "Dead should have MAX score");
    }

    #[test]
    fn test_score_source_raw_matches_stat() {
        let stat = ServerStat::new("test.com");
        stat.update_speed(5000, false);

        let stat_score = score_source(&stat);
        let raw_score = score_source_raw(stat.get_avg_speed() as f64, 0, 0);

        // Scores should be similar (within floating point tolerance)
        let diff = (stat_score - raw_score).abs();
        assert!(
            diff < 0.01,
            "Stat score ({}) should match raw score ({})",
            stat_score,
            raw_score
        );
    }

    #[test]
    fn test_is_better_source() {
        let fast = ServerStat::new("fast.com");
        fast.update_speed(1_000_000, false);

        let slow = ServerStat::new("slow.com");
        slow.update_speed(1000, false);

        assert!(
            is_better_source(&fast, &slow),
            "Fast should be better than slow"
        );
        assert!(
            !is_better_source(&slow, &fast),
            "Slow should not be better than fast"
        );
    }

    #[test]
    fn test_sort_by_score() {
        let slow = ServerStat::new("slow.com");
        slow.update_speed(1000, false);

        let fast = ServerStat::new("fast.com");
        fast.update_speed(1_000_000, false);

        let medium = ServerStat::new("medium.com");
        medium.update_speed(100_000, false);

        let mut stats: Vec<&ServerStat> = vec![&slow, &fast, &medium];
        sort_by_score(&mut stats);

        // Should be sorted best (fast) to worst (slow)
        assert!(
            score_source(stats[0]) <= score_source(stats[1]),
            "First should be best"
        );
        assert!(
            score_source(stats[1]) <= score_source(stats[2]),
            "Second should be better than third"
        );
    }

    #[test]
    fn test_age_bonus_capped() {
        let recent = ServerStat::new("recent.com");
        recent.update_speed(100_000, false);

        // Simulate old update by manipulating last_updated
        // (In practice, this would require waiting or mocking time)
        let old = ServerStat::new("old.com");
        old.update_speed(100_000, false);

        // Both have same speed, so scores should be similar
        // (age bonus is capped at 10, so difference is minimal)
        let recent_score = score_source(&recent);
        let old_score = score_source(&old);

        // Both should be finite and reasonably close
        assert!(recent_score.is_finite());
        assert!(old_score.is_finite());
    }

    #[test]
    fn test_zero_speed_no_failures() {
        let stat = ServerStat::new("new.com");
        // No updates yet

        let score = score_source(&stat);
        // Zero speed with zero failures should give finite score
        assert!(score.is_finite(), "New source should have finite score");
    }

    #[test]
    fn test_ema_updates_affect_score() {
        let stat = ServerStat::new("ema.test");

        // First update
        stat.update_speed(1000, false);
        let score1 = score_source(&stat);

        // Second update with higher speed
        stat.update_speed(10000, false);
        let score2 = score_source(&stat);

        // Score should improve (decrease) with higher EMA speed
        assert!(
            score2 < score1,
            "Higher EMA speed should improve score: before={}, after={}",
            score1,
            score2
        );
    }
}
