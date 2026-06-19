// Unified formatting utilities for human-readable display of bytes, speeds, and durations.
//
// All functions use IEC binary prefixes (KiB, MiB, GiB, TiB) for consistency
// across the codebase.

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;
const TIB: u64 = 1024 * GIB;

const SECS_PER_MINUTE: u64 = 60;
const SECS_PER_HOUR: u64 = 3600;

/// Format a byte count into human-readable form using IEC binary prefixes.
///
/// # Arguments
///
/// * `bytes` - Raw byte count
///
/// # Returns
///
/// Formatted string like `"12.3 MiB"` or `"456 KiB"`
///
/// # Example
///
/// ```
/// use aria2_core::util::format::format_bytes;
///
/// assert_eq!(format_bytes(500), "500 B");
/// assert_eq!(format_bytes(2048), "2.00 KiB");
/// assert_eq!(format_bytes(1048576), "1.00 MiB");
/// ```
pub fn format_bytes(bytes: u64) -> String {
    if bytes >= TIB {
        format!("{:.2} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Format a transfer speed into human-readable form using IEC binary prefixes.
///
/// # Arguments
///
/// * `bytes_per_sec` - Speed in bytes per second
///
/// # Returns
///
/// Formatted string like `"2.34 MiB/s"` or `"512 KiB/s"`
///
/// # Example
///
/// ```
/// use aria2_core::util::format::format_speed;
///
/// assert_eq!(format_speed(1536.0), "1.50 KiB/s");
/// assert_eq!(format_speed(1048576.0), "1.00 MiB/s");
/// ```
pub fn format_speed(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= TIB as f64 {
        format!("{:.2} TiB/s", bytes_per_sec / TIB as f64)
    } else if bytes_per_sec >= GIB as f64 {
        format!("{:.2} GiB/s", bytes_per_sec / GIB as f64)
    } else if bytes_per_sec >= MIB as f64 {
        format!("{:.2} MiB/s", bytes_per_sec / MIB as f64)
    } else if bytes_per_sec >= KIB as f64 {
        format!("{:.2} KiB/s", bytes_per_sec / KIB as f64)
    } else {
        format!("{:.0} B/s", bytes_per_sec)
    }
}

/// Format a duration as a human-readable string with "Xh Ym Zs" style.
///
/// # Arguments
///
/// * `secs` - Duration in seconds
///
/// # Returns
///
/// Formatted string like `"1h 23m 45s"`, `"5m 30s"`, or `"45s"`
///
/// # Example
///
/// ```
/// use aria2_core::util::format::format_duration;
///
/// assert_eq!(format_duration(45), "45s");
/// assert_eq!(format_duration(125), "2m 5s");
/// assert_eq!(format_duration(3661), "1h 1m 1s");
/// ```
pub fn format_duration(secs: u64) -> String {
    let hours = secs / SECS_PER_HOUR;
    let minutes = (secs % SECS_PER_HOUR) / SECS_PER_MINUTE;
    let seconds = secs % SECS_PER_MINUTE;

    if hours > 0 {
        format!("{}h {}m {}s", hours, minutes, seconds)
    } else if minutes > 0 {
        format!("{}m {}s", minutes, seconds)
    } else {
        format!("{}s", seconds)
    }
}

/// Format a duration as a compact string suitable for UI display.
///
/// Produces compact representations:
/// - Zero: `"0s"`
/// - Seconds only: `"42s"`
/// - Minutes + seconds: `"3m12s"`
/// - Hours + minutes + seconds: `"1h23m45s"`
///
/// # Arguments
///
/// * `secs` - Duration in seconds
///
/// # Returns
///
/// Short formatted string like `"3m12s"` or `"1h23m45s"`
///
/// # Example
///
/// ```
/// use aria2_core::util::format::format_duration_short;
///
/// assert_eq!(format_duration_short(0), "0s");
/// assert_eq!(format_duration_short(45), "45s");
/// assert_eq!(format_duration_short(125), "2m5s");
/// assert_eq!(format_duration_short(3661), "1h1m1s");
/// ```
pub fn format_duration_short(secs: u64) -> String {
    if secs == 0 {
        return "0s".to_string();
    }
    let h = secs / SECS_PER_HOUR;
    let m = (secs % SECS_PER_HOUR) / SECS_PER_MINUTE;
    let s = secs % SECS_PER_MINUTE;
    if h > 0 {
        format!("{}h{}m{}s", h, m, s)
    } else if m > 0 {
        format!("{}m{}s", m, s)
    } else {
        format!("{}s", s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(2048), "2.00 KiB");
        assert_eq!(format_bytes(1048576), "1.00 MiB");
        assert_eq!(format_bytes(1073741824), "1.00 GiB");
        assert_eq!(format_bytes(1099511627776), "1.00 TiB");
    }

    #[test]
    fn test_format_speed() {
        assert_eq!(format_speed(500.0), "500 B/s");
        assert_eq!(format_speed(1536.0), "1.50 KiB/s");
        assert_eq!(format_speed(1048576.0), "1.00 MiB/s");
        assert_eq!(format_speed(1073741824.0), "1.00 GiB/s");
        assert_eq!(format_speed(1099511627776.0), "1.00 TiB/s");
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(125), "2m 5s");
        assert_eq!(format_duration(3661), "1h 1m 1s");
    }

    #[test]
    fn test_format_duration_short() {
        assert_eq!(format_duration_short(0), "0s");
        assert_eq!(format_duration_short(1), "1s");
        assert_eq!(format_duration_short(59), "59s");
        assert_eq!(format_duration_short(60), "1m0s");
        assert_eq!(format_duration_short(61), "1m1s");
        assert_eq!(format_duration_short(3599), "59m59s");
        assert_eq!(format_duration_short(3600), "1h0m0s");
        assert_eq!(format_duration_short(3661), "1h1m1s");
    }
}
