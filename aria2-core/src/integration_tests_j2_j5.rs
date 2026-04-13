// Standalone tests for J2 and J5 features
// This avoids compilation issues in other parts of codebase

#[cfg(test)]
mod j2_j5_integration_tests {
    use crate::engine::http_segment_downloader::{
        ConnectionLimiter, calculate_dynamic_segment_size, score_source,
    };
    use crate::http::conditional_get::{
        ConditionalRequest, ResumeAction, SimpleDateTime, SmartResumeManager, handle_resume_status,
    };

    #[test]
    fn test_dynamic_segment_size_slow_start() {
        // Early download (elapsed < 2 seconds) should use conservative default
        let size = calculate_dynamic_segment_size(10_000_000, 4, 50000.0, 1);
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
        assert_eq!(
            size, 10_485_760,
            "Fast download should produce large segments"
        );

        // Very fast download (10 MB/s)
        let size_very_fast = calculate_dynamic_segment_size(1_000_000_000, 16, 10_485_760.0, 30);
        assert_eq!(
            size_very_fast, 16_777_216,
            "Very fast download should be capped at MAX_SEGMENT"
        );
    }

    #[test]
    fn test_connection_limiter_per_host() {
        let mut limiter = ConnectionLimiter::new(10, 2);

        assert!(
            limiter.try_acquire("example.com"),
            "First acquisition should succeed"
        );
        assert!(
            limiter.try_acquire("example.com"),
            "Second acquisition should succeed"
        );
        assert!(
            !limiter.try_acquire("example.com"),
            "Third acquisition should fail (per-host limit)"
        );

        assert!(
            limiter.try_acquire("other.com"),
            "Different host should work"
        );
        assert!(
            limiter.try_acquire("other.com"),
            "Second slot for other host"
        );
        assert!(
            !limiter.try_acquire("other.com"),
            "Third slot for other host should fail"
        );

        limiter.release("example.com");
        assert!(
            limiter.try_acquire("example.com"),
            "After release, should acquire again"
        );

        assert_eq!(
            limiter.available_for("example.com"),
            0,
            "No slots available after acquiring limit"
        );
        limiter.release("example.com");
        assert_eq!(
            limiter.available_for("example.com"),
            1,
            "One slot available after release"
        );
    }

    #[test]
    fn test_source_scoring_slow_penalized() {
        let fast_score = score_source(1_048_576.0, 0, 0);
        let slow_score = score_source(1024.0, 0, 0);
        let dead_score = score_source(0.0, 3, 0);

        assert!(
            slow_score > fast_score,
            "Slow source should have worse score than fast source"
        );
        assert_eq!(dead_score, f64::MAX, "Dead source should have MAX score");

        let failed_score = score_source(1_048_576.0, 2, 0);
        assert!(
            failed_score > fast_score,
            "Failed source should have worse score than successful one"
        );

        let recent_score = score_source(1_048_576.0, 0, 10);
        let old_score = score_source(1_048_576.0, 0, 300);
        assert!(
            old_score < recent_score,
            "Old success should give better (lower) score due to larger age bonus"
        );

        let very_slow = score_source(1024.0, 0, 0);
        assert!(
            recent_score < very_slow,
            "Even recent fast source beats slow source"
        );
    }

    #[test]
    fn test_conditional_headers_etag() {
        let mut cond = ConditionalRequest::new();
        cond.etag = Some("\"abc123\"".to_string());

        let headers = cond.to_headers();

        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "If-None-Match" && v == "\"abc123\"")
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "If-Match" && v == "\"abc123\"")
        );

        cond.update_from_response(
            200,
            &[("ETag".to_string(), "\"new-etag-value\"".to_string())],
        );
        assert_eq!(cond.etag, Some("new-etag-value".to_string()));
    }

    #[test]
    fn test_conditional_headers_last_modified() {
        let mut cond = ConditionalRequest::new();
        cond.last_modified = Some(SimpleDateTime::from_timestamp(784111777));

        let headers = cond.to_headers();

        assert!(headers.iter().any(|(k, _)| k == "If-Modified-Since"));
        assert!(headers.iter().any(|(k, _)| k == "If-Unmodified-Since"));

        let ims_header = headers
            .iter()
            .find(|(k, _)| k == "If-Modified-Since")
            .unwrap();
        assert!(ims_header.1.contains("GMT"), "Date should end with GMT");
        assert!(
            ims_header.1.contains(","),
            "Date should have comma after weekday"
        );
    }

    #[test]
    fn test_update_from_response_304() {
        let mut cond = ConditionalRequest::new();

        cond.update_from_response(
            304,
            &[
                ("ETag".to_string(), "\"static-content-v1\"".to_string()),
                (
                    "Last-Modified".to_string(),
                    "Mon, 01 Jan 2024 00:00:00 GMT".to_string(),
                ),
            ],
        );

        assert!(
            cond.should_skip(),
            "should_skip should be true after 304 response"
        );
        assert!(cond.not_modified, "not_modified flag should be set");

        assert_eq!(cond.etag, Some("static-content-v1".to_string()));
        assert!(cond.last_modified.is_some());
    }

    #[test]
    fn test_handle_status_416_needs_full_redownload() {
        let mut cond = ConditionalRequest::new();
        cond.content_length = Some(1000);

        let action = handle_resume_status(416, &cond);

        assert_eq!(
            action,
            ResumeAction::RedownloadFull,
            "416 should trigger RedownloadFull"
        );

        cond.update_from_response(416, &[]);
        assert!(
            cond.needs_full_redownload(),
            "After 416, needs_full_redownload should be true"
        );
        assert!(
            cond.content_length.is_none(),
            "content_length should be cleared after 416"
        );

        assert_eq!(
            handle_resume_status(304, &cond),
            ResumeAction::SkipUnchanged
        );
        assert_eq!(handle_resume_status(200, &cond), ResumeAction::Continue);
        assert_eq!(handle_resume_status(503, &cond), ResumeAction::RetryLater);
    }
}
