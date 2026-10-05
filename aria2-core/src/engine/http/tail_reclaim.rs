//! HTTP tail segment reclaim policy.
//!
//! Determines when an HTTP stream holding the tail (end) of a file should be
//! reclaimed because it has stalled. This is ported from the aria2-next C++
//! implementation (`HttpTailReclaimPolicy.cc`).
//!
//! The key insight: `is_http_tail_blocked` returns `true` when the HTTP tail
//! segment is *stuck* (i.e. conditions are met for potentially reclaiming it).
//! The `should_reclaim_http_tail_segment` function then adds a progress check
//! on top of that — only reclaiming when the tail is blocked **and** there has
//! been no progress for at least `stall_time`.

use std::time::Duration;

/// Captures the download state needed to decide whether the HTTP tail segment
/// should be reclaimed.
#[derive(Debug, Clone)]
pub struct HttpTailReclaimState {
    /// Protocol string — typically `"http"` or `"https"`.
    pub protocol: String,
    /// Whether a peer-to-peer protocol (BitTorrent, etc.) is involved.
    pub p2p_involved: bool,
    /// Total file length in bytes.
    pub total_length: i64,
    /// Pending (remaining) data length in bytes.
    pub pending_length: i64,
    /// Whether there are missing pieces that are not currently being used
    /// by any downloader.
    pub has_missing_unused_piece: bool,
    /// Number of concurrent download commands (all protocols).
    pub num_concurrent_command: i32,
    /// Number of stream (HTTP) download commands.
    pub num_stream_command: i32,
    /// Bytes downloaded in the current progress-measurement session.
    pub current_session_download_length: u64,
    /// Bytes downloaded in the previous progress-measurement session.
    pub last_session_download_length: u64,
    /// Elapsed time with no download progress.
    pub no_progress_time: Duration,
    /// Threshold after which a lack of progress is considered a stall.
    pub stall_time: Duration,
}

/// Returns `true` if the HTTP tail segment is *blocked* — meaning the
/// download is in a state where the tail segment may be stalled and a
/// reclaim should be considered.
///
/// A `false` return means the tail is **not** blocked; the conditions for
/// considering a reclaim are not met (e.g. not HTTP, p2p involved, unknown
/// file size, etc.).
///
/// Mirrors the C++ `isHttpTailBlocked` logic exactly:
///
/// | Condition                                 | → `false` (not blocked) |
/// |-------------------------------------------|-------------------------|
/// | Protocol is not HTTP/HTTPS                | yes                     |
/// | p2p is involved                           | yes                     |
/// | `total_length <= 0`                       | yes                     |
/// | `pending_length <= 0`                     | yes                     |
/// | `has_missing_unused_piece`                | yes                     |
/// | `num_concurrent_command <= 1`             | yes                     |
/// | `num_stream_command <= 0`                 | yes                     |
/// | `num_stream_command >= num_concurrent`    | yes                     |
/// | (none of the above)                       | → `true` (blocked)     |
pub fn is_http_tail_blocked(state: &HttpTailReclaimState) -> bool {
    if !is_http_protocol(&state.protocol)
        || state.p2p_involved
        || state.total_length <= 0
        || state.pending_length <= 0
        || state.has_missing_unused_piece
        || state.num_concurrent_command <= 1
        || state.num_stream_command <= 0
        || state.num_stream_command >= state.num_concurrent_command
    {
        return false;
    }
    true
}

/// Returns `true` if the HTTP tail segment should be reclaimed.
///
/// Reclaim is recommended when:
/// 1. The tail is blocked (`is_http_tail_blocked` is `true`), **and**
/// 2. There is no forward progress (`current_session_download_length <=
///    last_session_download_length`), **and**
/// 3. The no-progress duration has reached the stall threshold.
pub fn should_reclaim_http_tail_segment(state: &HttpTailReclaimState) -> bool {
    if !is_http_tail_blocked(state) {
        return false;
    }

    // Progress is being made — don't reclaim.
    if state.current_session_download_length > state.last_session_download_length {
        return false;
    }

    // No progress for at least stall_time → reclaim.
    state.no_progress_time >= state.stall_time
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Case-insensitive check for HTTP/HTTPS protocol.
fn is_http_protocol(protocol: &str) -> bool {
    protocol.eq_ignore_ascii_case("http") || protocol.eq_ignore_ascii_case("https")
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper that builds a default `HttpTailReclaimState` where the tail
    /// **is** blocked (all conditions favourable for reclaim consideration).
    fn blocked_state() -> HttpTailReclaimState {
        HttpTailReclaimState {
            protocol: "http".into(),
            p2p_involved: false,
            total_length: 10_000,
            pending_length: 5_000,
            has_missing_unused_piece: false,
            num_concurrent_command: 3,
            num_stream_command: 1,
            current_session_download_length: 100,
            last_session_download_length: 100,
            no_progress_time: Duration::from_secs(30),
            stall_time: Duration::from_secs(20),
        }
    }

    // ── is_http_protocol ────────────────────────────────────────────────

    #[test]
    fn test_is_http_protocol_lowercase() {
        assert!(is_http_protocol("http"));
        assert!(is_http_protocol("https"));
    }

    #[test]
    fn test_is_http_protocol_uppercase() {
        assert!(is_http_protocol("HTTP"));
        assert!(is_http_protocol("HTTPS"));
    }

    #[test]
    fn test_is_http_protocol_mixed_case() {
        assert!(is_http_protocol("HtTp"));
        assert!(is_http_protocol("HtTpS"));
    }

    #[test]
    fn test_is_http_protocol_non_http() {
        assert!(!is_http_protocol("ftp"));
        assert!(!is_http_protocol(""));
        assert!(!is_http_protocol("websocket"));
    }

    // ── is_http_tail_blocked — conditions that make it return false ─────

    #[test]
    fn test_blocked_returns_true_with_default_state() {
        assert!(is_http_tail_blocked(&blocked_state()));
    }

    #[test]
    fn test_not_blocked_when_non_http_protocol() {
        let mut s = blocked_state();
        s.protocol = "ftp".into();
        assert!(!is_http_tail_blocked(&s));
    }

    #[test]
    fn test_not_blocked_when_p2p_involved() {
        let mut s = blocked_state();
        s.p2p_involved = true;
        assert!(!is_http_tail_blocked(&s));
    }

    #[test]
    fn test_not_blocked_when_total_length_zero() {
        let mut s = blocked_state();
        s.total_length = 0;
        assert!(!is_http_tail_blocked(&s));
    }

    #[test]
    fn test_not_blocked_when_total_length_negative() {
        let mut s = blocked_state();
        s.total_length = -1;
        assert!(!is_http_tail_blocked(&s));
    }

    #[test]
    fn test_not_blocked_when_pending_length_zero() {
        let mut s = blocked_state();
        s.pending_length = 0;
        assert!(!is_http_tail_blocked(&s));
    }

    #[test]
    fn test_not_blocked_when_pending_length_negative() {
        let mut s = blocked_state();
        s.pending_length = -1;
        assert!(!is_http_tail_blocked(&s));
    }

    #[test]
    fn test_not_blocked_when_has_missing_unused_piece() {
        let mut s = blocked_state();
        s.has_missing_unused_piece = true;
        assert!(!is_http_tail_blocked(&s));
    }

    #[test]
    fn test_not_blocked_when_only_one_concurrent_command() {
        let mut s = blocked_state();
        s.num_concurrent_command = 1;
        assert!(!is_http_tail_blocked(&s));
    }

    #[test]
    fn test_not_blocked_when_zero_concurrent_commands() {
        let mut s = blocked_state();
        s.num_concurrent_command = 0;
        assert!(!is_http_tail_blocked(&s));
    }

    #[test]
    fn test_not_blocked_when_zero_stream_commands() {
        let mut s = blocked_state();
        s.num_stream_command = 0;
        assert!(!is_http_tail_blocked(&s));
    }

    #[test]
    fn test_not_blocked_when_stream_equals_concurrent() {
        // num_stream_command >= num_concurrent_command → not blocked
        let mut s = blocked_state();
        s.num_stream_command = 3;
        s.num_concurrent_command = 3;
        assert!(!is_http_tail_blocked(&s));
    }

    #[test]
    fn test_not_blocked_when_stream_exceeds_concurrent() {
        // num_stream_command > num_concurrent_command → not blocked
        let mut s = blocked_state();
        s.num_stream_command = 4;
        s.num_concurrent_command = 3;
        assert!(!is_http_tail_blocked(&s));
    }

    // ── is_http_tail_blocked — boundary conditions ──────────────────────

    #[test]
    fn test_blocked_with_https_protocol() {
        let mut s = blocked_state();
        s.protocol = "https".into();
        assert!(is_http_tail_blocked(&s));
    }

    #[test]
    fn test_blocked_with_https_uppercase() {
        let mut s = blocked_state();
        s.protocol = "HTTPS".into();
        assert!(is_http_tail_blocked(&s));
    }

    #[test]
    fn test_blocked_when_stream_less_than_concurrent() {
        // 1 < 3 → blocked condition holds
        let s = blocked_state();
        assert_eq!(s.num_stream_command, 1);
        assert_eq!(s.num_concurrent_command, 3);
        assert!(is_http_tail_blocked(&s));
    }

    // ── should_reclaim_http_tail_segment — tail not blocked ─────────────

    #[test]
    fn test_no_reclaim_when_tail_not_blocked() {
        // Non-HTTP → tail not blocked → no reclaim regardless of progress
        let mut s = blocked_state();
        s.protocol = "ftp".into();
        assert!(!should_reclaim_http_tail_segment(&s));
    }

    // ── should_reclaim_http_tail_segment — progress being made ──────────

    #[test]
    fn test_no_reclaim_when_progress_being_made() {
        let mut s = blocked_state();
        s.current_session_download_length = 200;
        s.last_session_download_length = 100;
        assert!(!should_reclaim_http_tail_segment(&s));
    }

    #[test]
    fn test_no_reclaim_when_progress_equal_sessions() {
        // current == last → no *additional* progress → continue to stall check
        let mut s = blocked_state();
        s.current_session_download_length = 100;
        s.last_session_download_length = 100;
        // no_progress_time >= stall_time → reclaim
        assert!(should_reclaim_http_tail_segment(&s));
    }

    // ── should_reclaim_http_tail_segment — stall detection ──────────────

    #[test]
    fn test_reclaim_when_stalled_long_enough() {
        let s = blocked_state();
        assert!(s.no_progress_time >= s.stall_time);
        assert!(should_reclaim_http_tail_segment(&s));
    }

    #[test]
    fn test_no_reclaim_when_not_stalled_long_enough() {
        let mut s = blocked_state();
        s.no_progress_time = Duration::from_secs(10);
        s.stall_time = Duration::from_secs(20);
        assert!(!should_reclaim_http_tail_segment(&s));
    }

    #[test]
    fn test_reclaim_when_stall_time_exactly_met() {
        let mut s = blocked_state();
        s.no_progress_time = Duration::from_secs(20);
        s.stall_time = Duration::from_secs(20);
        assert!(should_reclaim_http_tail_segment(&s));
    }

    // ── Combination / edge-case tests ───────────────────────────────────

    #[test]
    fn test_reclaim_blocked_zero_progress_stalled() {
        let mut s = blocked_state();
        s.current_session_download_length = 0;
        s.last_session_download_length = 0;
        s.no_progress_time = Duration::from_secs(60);
        s.stall_time = Duration::from_secs(30);
        assert!(should_reclaim_http_tail_segment(&s));
    }

    #[test]
    fn test_no_reclaim_blocked_but_still_progressing() {
        let mut s = blocked_state();
        s.current_session_download_length = 500;
        s.last_session_download_length = 100;
        s.no_progress_time = Duration::from_secs(60);
        s.stall_time = Duration::from_secs(30);
        // Progress happening → don't reclaim even though stalled timer is high
        assert!(!should_reclaim_http_tail_segment(&s));
    }

    #[test]
    fn test_no_reclaim_p2p_overrides_stall() {
        let mut s = blocked_state();
        s.p2p_involved = true;
        s.no_progress_time = Duration::from_secs(600);
        s.stall_time = Duration::from_secs(1);
        // p2p involved → tail not blocked → no reclaim
        assert!(!should_reclaim_http_tail_segment(&s));
    }

    #[test]
    fn test_no_reclaim_single_command_overrides_stall() {
        let mut s = blocked_state();
        s.num_concurrent_command = 1;
        s.no_progress_time = Duration::from_secs(600);
        s.stall_time = Duration::from_secs(1);
        assert!(!should_reclaim_http_tail_segment(&s));
    }

    #[test]
    fn test_no_reclaim_missing_unused_piece_overrides_stall() {
        let mut s = blocked_state();
        s.has_missing_unused_piece = true;
        s.no_progress_time = Duration::from_secs(600);
        s.stall_time = Duration::from_secs(1);
        assert!(!should_reclaim_http_tail_segment(&s));
    }

    #[test]
    fn test_no_reclaim_all_commands_are_streams() {
        let mut s = blocked_state();
        s.num_concurrent_command = 2;
        s.num_stream_command = 2;
        // stream >= concurrent → not blocked
        assert!(!should_reclaim_http_tail_segment(&s));
    }
}
