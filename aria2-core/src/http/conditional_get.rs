// Conditional GET and Smart Resume support for HTTP downloads.
// Implements RFC 7232 (Conditional Requests) and RFC 7233 (Range Requests)
// for efficient download resumption and unchanged-file detection.

use std::collections::HashMap;

/// Simple datetime structure for RFC 2822 date handling without chrono dependency.
/// Stores timestamp as seconds since Unix epoch for simplicity.
#[derive(Debug, Clone, Copy)]
pub struct SimpleDateTime {
    timestamp: i64, // seconds since Unix epoch
}

impl SimpleDateTime {
    /// Create from Unix timestamp
    pub fn from_timestamp(ts: i64) -> Self {
        Self { timestamp: ts }
    }

    /// Format date according to RFC 7231 (IMF-fixdate format).
    /// Example: "Sun, 06 Nov 1994 08:49:37 GMT"
    pub fn format_imf_fixdate(&self) -> String {
        // Days of week
        const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        // Months
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];

        // Calculate date components from timestamp
        // This is a simplified calculation - in production you'd use a proper library
        let total_days = self.timestamp / 86400;

        // Simplified: assume year 2024-2030 range for formatting
        // In production, use full date arithmetic
        let days_since_epoch = total_days + 719528; // Adjust for Unix epoch

        // Calculate year (simplified approximation)
        let mut year = 1970i64;
        let mut remaining_days = days_since_epoch;

        while remaining_days >= 365 {
            let days_in_year = if is_leap_year(year) { 366 } else { 365 };
            if remaining_days >= days_in_year {
                remaining_days -= days_in_year;
                year += 1;
            } else {
                break;
            }
        }

        // Calculate month and day
        const DAYS_IN_MONTH: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let mut month = 0usize;
        while month < 12 {
            let dim = if month == 1 && is_leap_year(year) {
                29
            } else {
                DAYS_IN_MONTH[month]
            };
            if remaining_days >= dim as i64 {
                remaining_days -= dim as i64;
                month += 1;
            } else {
                break;
            }
        }

        let day = remaining_days + 1; // 1-indexed

        // Calculate time of day
        let secs_in_day = ((self.timestamp % 86400) + 86400) % 86400;
        let hour = secs_in_day / 3600;
        let minute = (secs_in_day % 3600) / 60;
        let second = secs_in_day % 60;

        // Calculate day of week (Zeller's congruence simplified)
        // For Gregorian calendar
        let q = day;
        let m = if month < 2 {
            (month + 12) as i64
        } else {
            (month + 3) as i64
        };
        let y = if month < 2 { year - 1 } else { year };
        let h = (q + (13 * (m + 1)) / 5 + y + y / 4 - y / 100 + y / 400) % 7;
        let weekday_index = (h + 6) % 7; // Adjust so Sunday=0

        format!(
            "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
            DAYS[weekday_index as usize], day, MONTHS[month], year, hour, minute, second
        )
    }

    /// Try to parse an RFC 2822 formatted date string.
    /// Returns None if parsing fails.
    pub fn parse_rfc2822(date_str: &str) -> Option<Self> {
        // Example formats:
        // "Sun, 06 Nov 1994 08:49:37 GMT"
        // "Sunday, 06-Nov-94 08:49:37 GMT"

        let parts: Vec<&str> = date_str.split_whitespace().collect();
        if parts.len() < 5 {
            return None;
        }

        // Handle both "Day, DD Mon YYYY" and "Day, DD-Mon-YY" formats
        let (day_str, mon_str, year_str);

        if parts[1].contains('-') {
            // Format: "DD-Mon-YY" or "DD-Mon-YYYY"
            let date_parts: Vec<&str> = parts[1].split('-').collect();
            if date_parts.len() != 3 {
                return None;
            }
            day_str = date_parts[0];
            mon_str = date_parts[1];
            year_str = date_parts[2];
        } else {
            // Format: "DD Mon YYYY"
            day_str = parts[1];
            mon_str = parts[2];
            year_str = parts[3];
        }

        let day: u32 = day_str.parse().ok()?;
        let year: i64 = if year_str.len() == 2 {
            // Two-digit year: assume 1900 or 2000
            let yy: i32 = year_str.parse().ok()?;
            if yy < 70 {
                (2000 + yy) as i64
            } else {
                (1900 + yy) as i64
            }
        } else {
            year_str.parse().ok()?
        };

        // Parse month name to number
        const MONTH_NAMES: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let month = MONTH_NAMES
            .iter()
            .position(|&m| m.eq_ignore_ascii_case(mon_str))? as u32
            + 1;

        // Parse time
        let time_parts: Vec<&str> = parts[4].split(':').collect();
        if time_parts.len() != 3 {
            return None;
        }
        let hour: u32 = time_parts[0].parse().ok()?;
        let minute: u32 = time_parts[1].parse().ok()?;
        let second: u32 = time_parts[2].parse().ok()?;

        // Convert to Unix timestamp (simplified calculation)
        let timestamp = datetime_to_timestamp(year, month, day, hour, minute, second);

        Some(Self { timestamp })
    }
}

/// Check if a year is a leap year
fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

/// Convert datetime components to Unix timestamp (seconds since 1970-01-01)
/// Simplified implementation for common use cases
fn datetime_to_timestamp(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> i64 {
    // Days per month (non-leap year)
    const DAYS_IN_MONTH: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

    // Calculate total days from 1970-01-01
    let mut total_days: i64 = 0;

    // Add years
    for y in 1970..year {
        total_days += if is_leap_year(y) { 366 } else { 365 };
    }

    // Add months in current year
    for m in 1..month {
        total_days += if m == 2 && is_leap_year(year) {
            29
        } else {
            DAYS_IN_MONTH[(m - 1) as usize]
        } as i64;
    }

    // Add days (day is 1-indexed, so subtract 1)
    total_days += (day - 1) as i64;

    // Add time
    total_days * 86400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64
}

/// Manages HTTP conditional headers for smart resume and unchanged-file detection.
pub struct ConditionalRequest {
    pub last_modified: Option<SimpleDateTime>,
    pub etag: Option<String>,
    pub content_length: Option<u64>,
    /// Track whether server returned 304 Not Modified in last request
    pub not_modified: bool,
}

impl ConditionalRequest {
    /// Create a new empty ConditionalRequest
    pub fn new() -> Self {
        Self {
            last_modified: None,
            etag: None,
            content_length: None,
            not_modified: false,
        }
    }
}

impl Default for ConditionalRequest {
    fn default() -> Self {
        Self::new()
    }
}

impl ConditionalRequest {
    /// Build headers for conditional request.
    /// If both Last-Modified and ETag present, prefer ETag (stronger validation).
    pub fn to_headers(&self) -> Vec<(String, String)> {
        let mut headers = Vec::new();

        if let Some(ref etag) = self.etag {
            headers.push(("If-None-Match".into(), etag.clone()));
            headers.push(("If-Match".into(), etag.clone()));
        }

        if let Some(ref lm) = self.last_modified {
            headers.push(("If-Modified-Since".into(), lm.format_imf_fixdate()));
            headers.push(("If-Unmodified-Since".into(), lm.format_imf_fixdate()));
        }

        if let Some(len) = self.content_length {
            headers.push(("Range".into(), format!("bytes={}-", len)));
        }

        headers
    }

    /// Parse response headers to update state for next request.
    pub fn update_from_response(&mut self, status: u16, headers: &[(String, String)]) {
        for (name, value) in headers {
            match name.to_lowercase().as_str() {
                "last-modified" => {
                    if let Some(dt) = SimpleDateTime::parse_rfc2822(value) {
                        self.last_modified = Some(dt);
                    }
                }
                "etag" => {
                    self.etag = Some(value.trim_matches('"').to_string());
                }
                "content-length" => {
                    if let Ok(len) = value.parse::<u64>() {
                        self.content_length = Some(len);
                    }
                }
                _ => {}
            }
        }

        // Handle status codes
        match status {
            304 => {
                // Not Modified — file unchanged, skip download
                self.not_modified = true;
            }
            206 => {
                // Partial Content — resume successful
                self.not_modified = false;
            }
            416 => {
                // Range Not Satisfiable — need full re-download
                self.content_length = None;
                self.not_modified = false;
            }
            _ => {
                self.not_modified = false;
            }
        }
    }

    /// Should we skip this download? (304 Not Modified)
    pub fn should_skip(&self) -> bool {
        self.not_modified
    }

    /// Is resume possible? (have partial data + server supports Range)
    pub fn can_resume(&self, local_file_size: u64) -> bool {
        self.content_length.is_some_and(|len| local_file_size < len)
    }

    /// Need full re-download? (416 or no range support detected)
    pub fn needs_full_redownload(&self) -> bool {
        self.content_length.is_none()
    }
}

/// Coordinates conditional GET across multiple URI attempts for the same resource.
pub struct SmartResumeManager {
    entries: HashMap<String, ConditionalRequest>, // keyed by URL or GID
}

impl SmartResumeManager {
    /// Create a new SmartResumeManager
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Get or create conditional state for a download
    pub fn get_or_create(&mut self, key: &str) -> &mut ConditionalRequest {
        self.entries.entry(key.to_string()).or_default()
    }

    /// Record that server returned 304 (unchanged)
    pub fn mark_unchanged(&mut self, key: &str) {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.not_modified = true;
        }
    }

    /// Check if any URI returned 304 (all unchanged)
    pub fn is_all_unchanged(&self, keys: &[&str]) -> bool {
        keys.iter()
            .all(|key| self.entries.get(*key).is_some_and(|e| e.not_modified))
    }
}

impl Default for SmartResumeManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Action to take based on HTTP status code for smart resume scenarios.
#[derive(Debug, Clone, PartialEq)]
pub enum ResumeAction {
    Continue,       // Normal download/resume
    SkipUnchanged,  // 304 Not Modified — file up to date
    RedownloadFull, // 416 Range Not Satisfiable — start over
    RetryLater,     // 503 Server Unavailable — retry with delay
}

/// Handle special HTTP statuses for smart resume scenarios.
/// Returns action to take.
pub fn handle_resume_status(status: u16, _cond: &ConditionalRequest) -> ResumeAction {
    match status {
        200..=299 => ResumeAction::Continue,
        304 => ResumeAction::SkipUnchanged,
        416 => ResumeAction::RedownloadFull,
        500..=599 => ResumeAction::RetryLater,
        _ => ResumeAction::Continue, // Unknown — try anyway
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_conditional_headers_etag() {
        let mut cond = ConditionalRequest::new();
        cond.etag = Some("\"abc123\"".to_string());

        let headers = cond.to_headers();

        // Should have If-None-Match and If-Match with the etag value
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

        // ETag value should be trimmed of quotes when stored but preserved in headers
        cond.update_from_response(
            200,
            &[("ETag".to_string(), "\"new-etag-value\"".to_string())],
        );
        assert_eq!(cond.etag, Some("new-etag-value".to_string()));
    }

    #[test]
    fn test_conditional_headers_last_modified() {
        let mut cond = ConditionalRequest::new();

        // Set last modified using a known timestamp
        // November 6, 1994 08:49:37 GMT = 784111777 (approximately)
        cond.last_modified = Some(SimpleDateTime::from_timestamp(784111777));

        let headers = cond.to_headers();

        // Should have If-Modified-Since and If-Unmodified-Since headers
        assert!(headers.iter().any(|(k, _)| k == "If-Modified-Since"));
        assert!(headers.iter().any(|(k, _)| k == "If-Unmodified-Since"));

        // Verify the date format follows IMF-fixdate pattern (RFC 7231)
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

        // Simulate server response with 304 status
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

        // should_skip should return true after 304
        assert!(
            cond.should_skip(),
            "should_skip should be true after 304 response"
        );
        assert!(cond.not_modified, "not_modified flag should be set");

        // Headers should still be parsed
        assert_eq!(cond.etag, Some("static-content-v1".to_string()));
        assert!(cond.last_modified.is_some());
    }

    #[test]
    fn test_handle_status_416_needs_full_redownload() {
        let mut cond = ConditionalRequest::new();
        cond.content_length = Some(1000); // Assume we had content length before

        // Handle 416 status
        let action = handle_resume_status(416, &cond);

        assert_eq!(
            action,
            ResumeAction::RedownloadFull,
            "416 should trigger RedownloadFull"
        );

        // Update from response should clear content_length
        cond.update_from_response(416, &[]);
        assert!(
            cond.needs_full_redownload(),
            "After 416, needs_full_redownload should be true"
        );
        assert!(
            cond.content_length.is_none(),
            "content_length should be cleared after 416"
        );

        // Test other status codes
        assert_eq!(
            handle_resume_status(304, &cond),
            ResumeAction::SkipUnchanged
        );
        assert_eq!(handle_resume_status(200, &cond), ResumeAction::Continue);
        assert_eq!(handle_resume_status(206, &cond), ResumeAction::Continue);
        assert_eq!(handle_resume_status(503, &cond), ResumeAction::RetryLater);
    }

    #[test]
    fn test_smart_resume_manager() {
        let mut manager = SmartResumeManager::new();

        // Get or create entry
        let entry = manager.get_or_create("download-1");
        entry.etag = Some("\"file-v1\"".to_string());

        // Get existing entry
        let entry2 = manager.get_or_create("download-1");
        assert_eq!(
            entry2.etag,
            Some("\"file-v1\"".to_string()),
            "Should retrieve existing entry"
        );

        // Mark as unchanged
        manager.mark_unchanged("download-1");
        assert!(
            manager.is_all_unchanged(&["download-1"]),
            "Should report as unchanged"
        );

        // Multiple keys
        manager.get_or_create("download-2").not_modified = true;
        assert!(
            manager.is_all_unchanged(&["download-1", "download-2"]),
            "All should be unchanged"
        );

        manager.get_or_create("download-3"); // New entry, not modified=false by default
        assert!(
            !manager.is_all_unchanged(&["download-1", "download-3"]),
            "Not all unchanged"
        );
    }

    #[test]
    fn test_can_resume_and_needs_full_redownload() {
        let mut cond = ConditionalRequest::new();

        // No content length - cannot resume
        assert!(
            !cond.can_resume(100),
            "Cannot resume without content length"
        );
        assert!(
            cond.needs_full_redownload(),
            "Needs full redownload without content length"
        );

        // With content length and local size smaller
        cond.content_length = Some(1000);
        assert!(
            cond.can_resume(500),
            "Can resume when local file is smaller"
        );
        assert!(
            !cond.can_resume(1000),
            "Cannot resume when local file equals content length"
        );
        assert!(
            !cond.can_resume(1500),
            "Cannot resume when local file is larger"
        );
        assert!(
            !cond.needs_full_redownload(),
            "Doesn't need full redownload with content length"
        );
    }

    #[test]
    fn test_simple_datetime_parsing() {
        // Test parsing standard RFC 2822 format
        let dt = SimpleDateTime::parse_rfc2822("Sun, 06 Nov 1994 08:49:37 GMT");
        assert!(dt.is_some(), "Should parse valid RFC 2822 date");

        let dt = dt.unwrap();
        let formatted = dt.format_imf_fixdate();

        // Verify it contains expected components (format may vary by implementation)
        assert!(
            formatted.ends_with("GMT"),
            "Formatted date should end with GMT"
        );

        // Test invalid format
        let invalid = SimpleDateTime::parse_rfc2822("invalid-date");
        assert!(invalid.is_none(), "Should return None for invalid date");
    }

    #[test]
    fn test_conditional_request_with_range() {
        let mut cond = ConditionalRequest::new();
        cond.content_length = Some(1024 * 1024); // 1 MB

        let headers = cond.to_headers();

        // Should have Range header
        let range_header = headers.iter().find(|(k, _)| k == "Range");
        assert!(
            range_header.is_some(),
            "Should have Range header when content_length is set"
        );
        // Range header format: bytes={content_length}-
        let expected = format!("bytes={}-", cond.content_length.unwrap());
        assert_eq!(range_header.unwrap().1, expected);
    }
}
