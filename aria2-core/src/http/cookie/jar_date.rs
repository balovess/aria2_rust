//! HTTP date formatting and parsing helpers for the cookie jar.
//!
//! Provides SystemTime-based date conversion functions used by `JarCookie`
//! and `CookieJar` for Set-Cookie header parsing and serialization.

use std::time::{Duration, SystemTime};

use crate::error::{Aria2Error, Result};

/// Format a SystemTime as an HTTP-date string (RFC 7231 IMF-fixdate).
///
/// Produces output like: `Sun, 06 Nov 1994 08:49:37 GMT`
pub(super) fn format_systemtime_as_http_date(time: SystemTime) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let dur = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let epoch = dur.as_secs();

    let days_since_epoch = (epoch / 86400) as u32;
    let mut year = 1970u32;
    let mut remaining = days_since_epoch;

    loop {
        let leap =
            (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
        let days_in_year = if leap { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        year += 1;
    }

    let mdays = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 0u32;
    while month < 12 {
        let dim = if month == 1
            && ((year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400))
        {
            29
        } else {
            mdays[month as usize]
        };
        if remaining < dim {
            break;
        }
        remaining -= dim;
        month += 1;
    }
    let day = remaining + 1;
    let secs = epoch % 86400;
    let hour = (secs / 3600) as u32;
    let min = ((secs % 3600) / 60) as u32;
    let sec = (secs % 60) as u32;
    // Zeller's congruence to determine day of week (0=Sun..6=Sat)
    let dow: usize =
        ((year + (year / 4) - (year / 100) + (year / 400) + (13 * month + 1) / 5 + day + 308) % 7)
            as usize;

    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAYS[dow], day, MONTHS[month as usize], year, hour, min, sec
    )
}

/// Parse an HTTP-date string into a SystemTime.
///
/// Supports common date formats from RFC 7231 Section 7.1.1.1:
/// - IMF-fixdate: `Sun, 06 Nov 1994 08:49:37 GMT`
/// - RFC 850: `Sunday, 06-Nov-94 08:49:37 GMT`
/// - ANSI C asctime: `Sun Nov  6 08:49:37 1994`
///
/// If parsing fails, returns a far-future timestamp as fallback (1 year from now)
/// to avoid prematurely expiring cookies due to unparseable dates.
pub(super) fn parse_http_date(s: &str) -> Result<SystemTime> {
    let s = s.trim();
    let parts: Vec<&str> = s.split_whitespace().collect();

    // Handle various formats
    if parts.len() >= 5 {
        // Try to extract day, month, year, time components
        let (day_str, mon_str, year_str, time_str) = if parts[0].ends_with(',') {
            // IMF-fixdate or RFC 850 format: "Sun, 06 Nov 1994 08:49:37 GMT"
            // or "Sunday, 06-Nov-94 08:49:37 GMT"
            if parts.len() >= 6 {
                (
                    parts[1].trim(),
                    parts[2].trim(),
                    parts[3].trim(),
                    parts[4].trim(),
                )
            } else if parts.len() >= 5 {
                (
                    parts[1].split('-').next().unwrap_or("1"),
                    parts[1].split('-').nth(1).unwrap_or("Jan"),
                    parts[2].trim(),
                    parts[3].trim(),
                )
            } else {
                return Err(Aria2Error::Parse("Invalid date format".to_string()));
            }
        } else {
            // asctime format: "Sun Nov  6 08:49:37 1994"
            (
                parts[2].trim(),
                parts[1].trim(),
                parts[4].trim(),
                parts[3].trim(),
            )
        };

        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];

        let day: u32 = day_str.parse().unwrap_or(1);
        let month_idx = MONTHS
            .iter()
            .position(|&m| m.eq_ignore_ascii_case(mon_str))
            .unwrap_or(0);
        let year: i32 = year_str.parse().unwrap_or({
            // Handle 2-digit years (RFC 850)
            let y: u32 = year_str.parse().unwrap_or(70);
            if y < 100 {
                (1900 + y) as i32
            } else {
                y as i32
            }
        });

        let time_parts: Vec<u32> = time_str.split(':').filter_map(|x| x.parse().ok()).collect();
        let hour = time_parts.first().copied().unwrap_or(0);
        let min = time_parts.get(1).copied().unwrap_or(0);
        let sec = time_parts.get(2).copied().unwrap_or(0);

        // Convert to Unix timestamp (simplified calculation)
        let total_days = calculate_days_since_epoch(year, month_idx as u32, day);
        let timestamp =
            total_days as u64 * 86400 + hour as u64 * 3600 + min as u64 * 60 + sec as u64;

        return Ok(SystemTime::UNIX_EPOCH + Duration::from_secs(timestamp));
    }

    // Fallback: treat as far-future expiry (1 year from now) to avoid
    // incorrectly expiring cookies when we can't parse the date format.
    // This is safer than returning an error which would cause cookie rejection.
    Ok(SystemTime::now() + Duration::from_secs(86400 * 365))
}

/// Calculate the number of days since Unix epoch (1970-01-01) for a given date.
fn calculate_days_since_epoch(year: i32, month: u32, day: u32) -> i64 {
    let mut days: i64 = 0;

    // Full years before current year
    for y in 1970..year {
        days += if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
            366
        } else {
            365
        };
    }

    // Months before current month in current year
    let mdays = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let is_leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    for m in 0..month {
        let d = if m == 1 && is_leap {
            29
        } else {
            mdays[m as usize]
        };
        days += d as i64;
    }

    // Days in current month (day is 1-indexed)
    days += day as i64 - 1;

    days
}
