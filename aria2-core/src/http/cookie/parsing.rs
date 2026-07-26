//! RFC 6265 helper functions for cookie domain/path matching and date parsing.

use std::time::{SystemTime, UNIX_EPOCH};

/// Check whether `host` domain-matches `domain` per RFC 6265 Section 5.1.3.
///
/// A string domain-matches a given domain string if at least one of the
/// following conditions holds:
/// 1. The host string and the domain string are identical.
/// 2. All of the following conditions hold:
///    a. The host string is a host name (not an IP)
///    b. The domain string is a suffix of the host string
///    c. The last character of the host that is not included in the domain
///       string is a `.` character.
pub(crate) fn domain_matches(host: &str, domain: &str) -> bool {
    let h = host.to_lowercase();
    let d = domain.to_lowercase();

    if h == d {
        return true;
    }

    // Numeric hosts cannot receive domain-scoped cookies (per C++ aria2 behavior)
    if is_numeric_host(host) {
        return false;
    }

    // Host must end with "." + domain for subdomain matching
    h.ends_with(&format!(".{}", d))
}

/// Check whether `cookie_path` path-matches `request_path` per RFC 6265 Section 5.1.4.
///
/// A request-path path-matches a given cookie-path if:
/// 1. The cookie-path is identical to the request-path, OR
/// 2. The cookie-path is a prefix of the request-path, AND
///    the last character of the cookie-path is `/`, OR
///    the first character of the request-path that is not included in the
///    cookie-path is a `/` character.
pub(crate) fn path_matches(cookie_path: &str, request_path: &str) -> bool {
    // Identical paths always match
    if cookie_path == request_path {
        return true;
    }

    // Cookie path must be a prefix of request path
    if !request_path.starts_with(cookie_path) {
        return false;
    }

    // Cookie path ends with / -> match
    if cookie_path.ends_with('/') {
        return true;
    }

    // The first character of request_path not included in cookie_path must be /
    let remaining = &request_path[cookie_path.len()..];
    remaining.starts_with('/')
}

/// Check whether a host string is a numeric IP address (IPv4 or IPv6).
///
/// Per C++ aria2 behavior, numeric hosts cannot receive domain-scoped cookies.
pub(crate) fn is_numeric_host(host: &str) -> bool {
    // IPv4: all characters are digits or dots
    let is_ipv4 = host.chars().all(|c| c.is_ascii_digit() || c == '.');
    if is_ipv4 && host.contains('.') {
        return true;
    }
    // IPv6: contains colons
    if host.contains(':') {
        return true;
    }
    false
}

/// Return the current time as Unix epoch seconds.
pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Format a Unix epoch timestamp as an HTTP-date per RFC 7231 Section 7.1.1.1.
///
/// Example output: `"Wed, 09 Jun 2021 10:18:14 GMT"`
pub(crate) fn format_http_date(epoch: i64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let epoch = epoch as u64;
    let days_since_epoch = epoch / 86400;
    let mut y = 1970u32;
    let mut remaining = days_since_epoch as u32;
    loop {
        let leap = y.is_multiple_of(4) && !y.is_multiple_of(100) || y.is_multiple_of(400);
        let days_in_year = if leap { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let mdays = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0u32;
    while m < 12 {
        let dim =
            if m == 1 && (y.is_multiple_of(4) && !y.is_multiple_of(100) || y.is_multiple_of(400)) {
                29
            } else {
                mdays[m as usize]
            };
        if remaining < dim {
            break;
        }
        remaining -= dim;
        m += 1;
    }
    let day = remaining + 1;
    let secs = epoch % 86400;
    let hour = secs / 3600;
    let min = (secs % 3600) / 60;
    let sec = secs % 60;
    let dow = ((y + (y / 4) - (y / 100) + (y / 400) + (13 * m + 1) / 5 + day + 308) % 7) as usize;
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAYS[dow], day, MONTHS[m as usize], y, hour, min, sec
    )
}

/// Parse an HTTP-date string into a Unix epoch timestamp.
///
/// Implements the full RFC 6265 Section 5.1.1 date parsing algorithm,
/// matching the C++ `cookie_helper.cc::parseDate()` behavior:
/// 1. Split the date string into tokens by delimiter characters
///    (tab, space, and most punctuation)
/// 2. Identify the time token (HH:MM:SS), day-of-month token, month token,
///    and year token regardless of their position in the string
/// 3. Normalize 2-digit years per RFC 6265 (70-99 -> 1970-1999, 0-69 -> 2000-2069)
/// 4. Validate the date (day-of-month ranges, leap year, etc.)
/// 5. Convert to Unix epoch using UTC (equivalent to C++ timegm())
///
/// Returns `None` if the date string cannot be parsed.
pub(crate) fn parse_http_date(s: &str) -> Option<i64> {
    const MONTH_NAMES: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];

    // Step 1: Tokenize by delimiter characters per RFC 6265 Section 5.1.1.
    // Delimiters: 0x09, 0x20-0x2F, 0x3B-0x40, 0x5B-0x60, 0x7B-0x7E
    fn is_delimiter(c: u8) -> bool {
        c == 0x09
            || (0x20..=0x2F).contains(&c)
            || (0x3B..=0x40).contains(&c)
            || (0x5B..=0x60).contains(&c)
            || (0x7B..=0x7E).contains(&c)
    }

    let bytes = s.trim().as_bytes();
    let mut tokens: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if is_delimiter(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && !is_delimiter(bytes[i]) {
            i += 1;
        }
        tokens.push(std::str::from_utf8(&bytes[start..i]).ok()?);
    }

    // Step 2: Identify time, day-of-month, month, and year tokens.
    let mut found_time = false;
    let mut hour: u32 = 0;
    let mut minute: u32 = 0;
    let mut second: u32 = 0;

    let mut found_day = false;
    let mut day_of_month: u32 = 0;

    let mut found_month = false;
    let mut month: u32 = 0; // 1-based

    let mut found_year = false;
    let mut year: u32 = 0;

    for token in &tokens {
        // Try to parse as time (HH:MM:SS)
        if !found_time {
            if let Some((h, m, s)) = parse_time_token(token) {
                hour = h;
                minute = m;
                second = s;
                found_time = true;
                continue;
            }
        }

        // Try to parse as day-of-month (1-2 digit number)
        if !found_day {
            let digits = leading_digits(token);
            if (1..=2).contains(&digits) && digits == token.len() {
                if let Ok(d) = token.parse::<u32>() {
                    day_of_month = d;
                    found_day = true;
                    continue;
                }
            }
        }

        // Try to parse as month name (case-insensitive, at least 3 chars)
        if !found_month && token.len() >= 3 {
            let lower = token.to_lowercase();
            for (idx, &name) in MONTH_NAMES.iter().enumerate() {
                if lower.starts_with(name) {
                    month = (idx + 1) as u32;
                    found_month = true;
                    break;
                }
            }
            if found_month {
                continue;
            }
        }

        // Try to parse as year (1-4 digit number)
        if !found_year {
            let digits = leading_digits(token);
            if (1..=4).contains(&digits) && digits == token.len() {
                if let Ok(y) = token.parse::<u32>() {
                    year = y;
                    found_year = true;
                    continue;
                }
            }
        }
    }

    // Step 3: Normalize 2-digit years per RFC 6265 Section 5.1.1
    if (70..=99).contains(&year) {
        year += 1900;
    } else if year <= 69 {
        year += 2000;
    }

    // Step 4: Validate the date
    if !found_time || !found_day || !found_month || !found_year {
        return None;
    }
    if !(1..=31).contains(&day_of_month) || year < 1601 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    // Validate day-of-month against month
    let max_day = match month {
        4 | 6 | 9 | 11 => 30,
        2 => {
            let is_leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if is_leap { 29 } else { 28 }
        }
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return None,
    };
    if day_of_month > max_day {
        return None;
    }

    // Step 5: Convert to Unix epoch timestamp (equivalent to C++ timegm())
    let total_days = days_since_epoch(year, month, day_of_month);
    Some(total_days as i64 * 86400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64)
}

/// Parse a time token in HH:MM:SS format.
/// Returns (hour, minute, second) if valid, None otherwise.
fn parse_time_token(token: &str) -> Option<(u32, u32, u32)> {
    let parts: Vec<&str> = token.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    // Each component must be 1-2 digits
    for p in &parts {
        let digits = leading_digits(p);
        if digits == 0 || digits > 2 || digits != p.len() {
            return None;
        }
    }
    let h: u32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let s: u32 = parts[2].parse().ok()?;
    Some((h, m, s))
}

/// Count leading ASCII digits in a string.
fn leading_digits(s: &str) -> usize {
    s.bytes().take_while(|c| c.is_ascii_digit()).count()
}

/// Calculate the number of days since Unix epoch (1970-01-01 UTC) for a given date.
/// Equivalent to C++ timegm() for date conversion.
fn days_since_epoch(year: u32, month: u32, day: u32) -> u32 {
    let mut days: u32 = 0;
    for y in 1970..year {
        days += if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
            366
        } else {
            365
        };
    }
    let mdays = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let is_leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    for m in 1..month {
        days += if m == 2 && is_leap {
            29
        } else {
            mdays[(m - 1) as usize]
        };
    }
    days + day - 1
}
