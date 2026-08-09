//! Source scoring functions for adaptive URI selection.
//!
//! These functions score server mirrors based on speed, failure history,
//! and recency of success. Lower score = better source. Used by
//! [`AdaptiveUriSelector`](super::AdaptiveUriSelector) for mirror ranking.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::selector::server_stat::ServerStat;

/// Score a source server using EMA-averaged speed from ServerStat.
///
/// Lower score = better source. `f64::MAX` indicates a dead source.
pub fn score_source(stat: &ServerStat) -> f64 {
    let avg_speed = stat.get_avg_speed() as f64;
    let failure_count = stat.get_consecutive_failures();
    let last_success_age = calculate_last_success_age(stat);

    if avg_speed <= 0.0 && failure_count > 0 {
        return f64::MAX;
    }

    let speed_score = if avg_speed > 0.0 {
        -avg_speed.ln_1p()
    } else {
        0.0
    };

    let penalty = (failure_count as f64) * 100.0;
    let age_bonus = (last_success_age as f64 / 60.0).min(10.0);

    speed_score + penalty - age_bonus
}

/// Score a source using raw parameters (convenience function).
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
pub fn is_better_source(stat_a: &ServerStat, stat_b: &ServerStat) -> bool {
    score_source(stat_a) < score_source(stat_b)
}

/// Sort a list of ServerStat references by score (best first).
pub fn sort_by_score(stats: &mut [&ServerStat]) {
    stats.sort_by(|a, b| {
        let score_a = score_source(a);
        let score_b = score_source(b);
        score_a
            .partial_cmp(&score_b)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_score_source_zero_speed_no_failures() {
        let stat = ServerStat::new("test.com");
        let score = score_source(&stat);
        // Zero speed, zero failures → speed_score=0, penalty=0, age_bonus=0
        assert_eq!(score, 0.0);
    }

    #[test]
    fn test_score_source_dead() {
        let mut stat = ServerStat::new("dead.com");
        stat.consecutive_failures = 1;
        let score = score_source(&stat);
        assert_eq!(score, f64::MAX, "Dead source should score f64::MAX");
    }

    #[test]
    fn test_score_source_with_speed() {
        let stat = ServerStat::new("fast.com");
        stat.increment_counter();
        stat.update_speed(10000, false);
        let score = score_source(&stat);
        // Should be negative (good score): ln_1p(10000) ≈ 9.21
        assert!(score < 0.0, "Fast server should have negative (good) score");
    }

    #[test]
    fn test_better_source_prefers_faster() {
        let fast = ServerStat::new("fast.com");
        fast.increment_counter();
        fast.update_speed(10000, false);
        let slow = ServerStat::new("slow.com");
        slow.increment_counter();
        slow.update_speed(100, false);
        assert!(is_better_source(&fast, &slow));
    }

    #[test]
    fn test_score_source_raw_matches() {
        let stat = ServerStat::new("raw.com");
        stat.increment_counter();
        stat.update_speed(5000, false);
        let score_fn = score_source(&stat);
        let score_raw = score_source_raw(5000.0, 0, 0);
        // Should be approximately equal (may differ slightly due to EMA)
        assert!((score_fn - score_raw).abs() < 100.0);
    }
}
