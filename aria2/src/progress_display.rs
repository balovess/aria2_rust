//! Progress Display - Download progress and status output
//!
//! This module provides utilities for displaying download progress,
//! status information, and formatted output to the console.
//!
//! # Features
//!
//! - Formatted speed display (KB/s, MB/s)
//! - Progress bar rendering
//! - Colorized status messages
//! - Summary statistics formatting
//!
//! # Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/ConsoleStatCalc.cc/h` - Console statistics calculation
//! - `src/DownloadHandler.cc/h` - Download status display

use colored::Colorize;
use std::time::Duration;

/// Format bytes into human-readable string
///
/// # Arguments
/// * `bytes` - Number of bytes
///
/// # Returns
/// * Formatted string (e.g., "1.23 MB", "456 KB")
pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Format download speed with unit
///
/// # Arguments
/// * `bytes_per_second` - Download speed in bytes/sec
///
/// # Returns
/// * Formatted speed string (e.g., "1.23 MB/s")
pub fn format_speed(bytes_per_second: f64) -> String {
    if bytes_per_second < 1024.0 {
        format!("{:.2} B/s", bytes_per_second)
    } else if bytes_per_second < 1024.0 * 1024.0 {
        format!("{:.2} KB/s", bytes_per_second / 1024.0)
    } else if bytes_per_second < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} MB/s", bytes_per_second / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB/s", bytes_per_second / (1024.0 * 0x400_000u64 as f64))
    }
}

/// Format duration in human-readable form
///
/// # Arguments
/// * `duration` - Time duration
///
/// # Returns
/// * Formatted duration string (e.g., "1h 23m 45s", "5m 30s")
pub fn format_duration(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;

    if hours > 0 {
        format!("{}h {}m {}s", hours, minutes, seconds)
    } else if minutes > 0 {
        format!("{}m {}s", minutes, seconds)
    } else {
        format!("{}s", seconds)
    }
}

/// Render a simple text-based progress bar
///
/// # Arguments
/// * `progress` - Progress value between 0.0 and 1.0
/// * `width` - Width of the bar in characters (default: 40)
///
/// # Returns
/// * String representation of the progress bar
pub fn render_progress_bar(progress: f64, width: usize) -> String {
    let clamped = progress.max(0.0).min(1.0);
    let filled = (clamped * width as f64).round() as usize;
    let empty = width - filled;

    let bar: String = "█".repeat(filled) + &"░".repeat(empty);
    format!("[{}] {:.1}%", bar, clamped * 100.0)
}

/// Print a download summary header
pub fn print_summary_header() {
    println!("{}", "=== aria2-rust Download Summary ===".cyan().bold());
}

/// Print a single download entry summary
///
/// # Arguments
/// * `index` - Entry number
/// * `gid` - Download GID
/// * `status` - Status string ("complete", "error", etc.)
/// * `total_size` - Total file size in bytes
/// * `downloaded_size` - Downloaded size in bytes
/// * `speed` - Average download speed (bytes/sec)
/// * `elapsed` - Time taken for the download
pub fn print_download_entry(
    index: usize,
    gid: u64,
    status: &str,
    total_size: u64,
    downloaded_size: u64,
    speed: f64,
    elapsed: Duration,
) {
    let status_colored = match status {
        "complete" => status.green().bold(),
        "error" => status.red().bold(),
        "running" => status.yellow(),
        _ => status.normal(),
    };

    println!(
        "#{:<3} | GID: {:<16x} | Status: {} | Size: {}/{} | Speed: {} | Time: {}",
        index.to_string().blue(),
        gid,
        status_colored,
        format_bytes(downloaded_size),
        format_bytes(total_size),
        format_speed(speed),
        format_duration(elapsed)
    );
}

/// Print final statistics summary
///
/// # Arguments
/// * `total_files` - Total number of files processed
/// * `total_bytes` - Total bytes downloaded
/// * `total_time` - Total time elapsed
/// * `success_count` - Number of successful downloads
/// * `error_count` - Number of failed downloads
pub fn print_final_stats(
    total_files: usize,
    total_bytes: u64,
    total_time: Duration,
    success_count: usize,
    error_count: usize,
) {
    println!();
    println!("{}", "=== Final Statistics ===".cyan().bold());
    println!(
        "  Total Files:     {}",
        total_files.to_string().white()
    );
    println!(
        "  Total Downloaded: {}",
        format_bytes(total_bytes).green()
    );
    println!(
        "  Total Time:      {}",
        format_duration(total_time).white()
    );
    println!(
        "  Successful:      {}",
        success_count.to_string().green()
    );
    println!(
        "  Failed:          {}",
        if error_count > 0 {
            error_count.to_string().red()
        } else {
            error_count.to_string().white()
        }
    );

    if total_time.as_secs() > 0 {
        let avg_speed = total_bytes as f64 / total_time.as_secs_f64();
        println!(
            "  Average Speed:   {}",
            format_speed(avg_speed).yellow()
        );
    }

    println!();
}

/// Print an error message with red color
pub fn print_error(message: &str) {
    eprintln!("{} {}", "ERROR:".red().bold(), message.red());
}

/// Print a warning message with yellow color
pub fn print_warning(message: &str) {
    eprintln!("{} {}", "WARNING:".yellow().bold(), message.yellow());
}

/// Print an info message with blue color
pub fn print_info(message: &str) {
    println!("{} {}", "INFO:".blue(), message.blue());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes_bytes() {
        assert_eq!(format_bytes(500), "500 B");
    }

    #[test]
    fn test_format_bytes_kb() {
        assert_eq!(format_bytes(2048), "2.00 KB");
    }

    #[test]
    fn test_format_bytes_mb() {
        assert_eq!(format_bytes(1048576), "1.00 MB");
    }

    #[test]
    fn test_format_bytes_gb() {
        assert_eq!(format_bytes(1073741824), "1.00 GB");
    }

    #[test]
    fn test_format_speed_bps() {
        let result = format_speed(512.0);
        assert!(result.contains("B/s"));
    }

    #[test]
    fn test_format_speed_kbps() {
        let result = format_speed(2048.0);
        assert!(result.contains("KB/s"));
    }

    #[test]
    fn test_format_duration_seconds() {
        let result = format_duration(Duration::from_secs(45));
        assert_eq!(result, "45s");
    }

    #[test]
    fn test_format_duration_minutes() {
        let result = format_duration(Duration::from_secs(125));
        assert_eq!(result, "2m 5s");
    }

    #[test]
    fn test_format_duration_hours() {
        let result = format_duration(Duration::from_secs(3661));
        assert!(result.starts_with("1h"));
    }

    #[test]
    fn test_render_progress_bar_zero() {
        let result = render_progress_bar(0.0, 10);
        assert!(result.contains("0.0%"));
        assert!(result.contains("░")); // Empty bars
    }

    #[test]
    fn test_render_progress_bar_full() {
        let result = render_progress_bar(1.0, 10);
        assert!(result.contains("100.0%"));
        assert!(result.contains("█")); // Filled bars
    }

    #[test]
    fn test_render_progress_bar_half() {
        let result = render_progress_bar(0.5, 10);
        assert!(result.contains("50.0%"));
    }

    #[test]
    fn test_print_functions_dont_panic() {
        // Just ensure they don't panic
        print_summary_header();
        print_download_entry(
            1,
            0x12345678,
            "complete",
            1024,
            1024,
            1024.0,
            Duration::from_secs(10),
        );
        print_final_stats(5, 5000, Duration::from_secs(100), 4, 1);
        print_error("Test error");
        print_warning("Test warning");
        print_info("Test info");
    }
}
