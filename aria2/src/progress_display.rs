//! Progress Display - Download progress and status output
//!
//! This module provides utilities for displaying download progress,
//! status information, and formatted output to the console.
//!
//! # Features
//!
//! - Formatted speed display (KiB/s, MiB/s)
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

// Re-export shared formatting functions from aria2-core
pub use aria2_core::util::format::{format_bytes, format_duration, format_speed};

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
        format_duration(elapsed.as_secs())
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
        format_duration(total_time.as_secs()).white()
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
    eprintln!("{} {}", crate::constants::LABEL_ERROR.red().bold(), message.red());
}

/// Print a warning message with yellow color
pub fn print_warning(message: &str) {
    eprintln!("{} {}", "WARNING:".yellow().bold(), message.yellow());
}

/// Print an info message with blue color
pub fn print_info(message: &str) {
    println!("{} {}", crate::constants::LABEL_INFO.blue(), message.blue());
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
        assert_eq!(format_bytes(2048), "2.00 KiB");
    }

    #[test]
    fn test_format_bytes_mb() {
        assert_eq!(format_bytes(1048576), "1.00 MiB");
    }

    #[test]
    fn test_format_bytes_gb() {
        assert_eq!(format_bytes(1073741824), "1.00 GiB");
    }

    #[test]
    fn test_format_speed_bps() {
        let result = format_speed(512.0);
        assert!(result.contains("B/s"));
    }

    #[test]
    fn test_format_speed_kbps() {
        let result = format_speed(2048.0);
        assert!(result.contains("KiB/s"));
    }

    #[test]
    fn test_format_duration_seconds() {
        let result = format_duration(45);
        assert_eq!(result, "45s");
    }

    #[test]
    fn test_format_duration_minutes() {
        let result = format_duration(125);
        assert_eq!(result, "2m 5s");
    }

    #[test]
    fn test_format_duration_hours() {
        let result = format_duration(3661);
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
