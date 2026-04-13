//! Console Progress Bar TUI for aria2-rust CLI
//!
//! Provides a terminal-based progress display system that renders
//! download task progress with visual bars, speed indicators, and
//! overall summary statistics.
//!
//! # Display Format
//!
//! Per-task:
//! ```text
//! [#1] filename.iso
//!      [████████████░░░░░░░] 65.2%  (12.3MiB / 18.9MiB)  DL:2.34MiB/s  ETA:3m12s
//! ```
//!
//! Overall summary:
//! ```text
//! Overall: [████████░░░░░░░░░░] 42%  (450MiB / 1.07GiB)  DL:5.67MiB/s  3 active / 8 total
//! ```

use std::time::{Duration, Instant};

/// Status of a single download task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    /// Task is actively downloading/uploading
    Active,
    /// Task is queued and waiting to start
    Waiting,
    /// Download completed successfully
    Complete,
    /// Task encountered an error
    Error,
    /// Task is in seeding mode (BT only)
    Seeding,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskStatus::Active => write!(f, "ACTIVE"),
            TaskStatus::Waiting => write!(f, "WAITING"),
            TaskStatus::Complete => write!(f, "COMPLETE"),
            TaskStatus::Error => write!(f, "ERROR"),
            TaskStatus::Seeding => write!(f, "SEEDING"),
        }
    }
}

/// Progress data for a single download task.
///
/// Contains all fields needed to render the task's progress bar line(s).
pub struct TaskProgress {
    /// Global identifier for this task
    pub gid: String,
    /// Filename being downloaded (display name)
    pub filename: String,
    /// Total file size in bytes
    pub total_length: u64,
    /// Bytes downloaded so far
    pub completed_length: u64,
    /// Current download speed in bytes/second
    pub download_speed: f64,
    /// Current upload speed in bytes/second
    pub upload_speed: f64,
    /// Whether this is a BitTorrent task (affects display)
    pub is_bt: bool,
    /// Number of connected seeders (BT only)
    pub num_seeders: usize,
    /// Number of connected peers (BT only)
    pub num_peers: usize,
    /// Total bytes uploaded (BT only)
    pub uploaded: u64,
    /// Current status of the task
    pub status: TaskStatus,
}

/// Main progress bar renderer for aria2-rust CLI.
///
/// Manages multiple tasks, tracks timing, and produces formatted output
/// strings suitable for terminal display with ANSI escape codes or plain text.
pub struct ProgressBar {
    /// If true, suppress all rendering output
    quiet: bool,
    /// Width of the progress bar portion (in characters)
    width: usize,
    /// List of tracked tasks
    tasks: Vec<TaskProgress>,
    /// Timestamp when ProgressBar was created
    started: Instant,
    /// Timestamp of last render call (for rate limiting)
    last_render: Instant,
    /// Minimum interval between renders
    render_interval: Duration,
}

impl ProgressBar {
    /// Create a new ProgressBar instance.
    ///
    /// # Arguments
    ///
    /// * `quiet` - If true, `render()` always returns an empty string
    ///
    /// # Example
    ///
    /// ```
    /// use aria2::ui::progress_bar::ProgressBar;
    /// let bar = ProgressBar::new(false);
    /// ```
    pub fn new(quiet: bool) -> Self {
        Self {
            quiet,
            width: 24, // Default bar width in characters
            tasks: Vec::new(),
            started: Instant::now(),
            last_render: Instant::now() - Duration::from_secs(1), // Allow immediate first render
            render_interval: Duration::from_millis(250),          // ~4 FPS max
        }
    }

    /// Set the width of the progress bar (in characters).
    ///
    /// Default is 24 characters. Minimum effective value is 4.
    pub fn with_width(mut self, width: usize) -> Self {
        self.width = width.max(4);
        self
    }

    /// Set the minimum interval between render calls.
    ///
    /// Default is 250ms (~4 FPS). Used for rate-limiting terminal updates.
    pub fn with_render_interval(mut self, interval: Duration) -> Self {
        self.render_interval = interval;
        self
    }

    /// Add a new task to track.
    ///
    /// # Arguments
    ///
    /// * `task` - The task progress data to start tracking
    pub fn add_task(&mut self, task: TaskProgress) {
        self.tasks.push(task);
    }

    /// Remove a task by its GID.
    ///
    /// # Arguments
    ///
    /// * `gid` - The global identifier of the task to remove
    ///
    /// # Returns
    ///
    /// * `true` if a task was found and removed, `false` otherwise
    pub fn remove_task(&mut self, gid: &str) -> bool {
        let original_len = self.tasks.len();
        self.tasks.retain(|t| t.gid != gid);
        self.tasks.len() < original_len
    }

    /// Update an existing task by GID using a closure.
    ///
    /// # Arguments
    ///
    /// * `gid` - The global identifier of the task to update
    /// * `updater` - Closure that receives a mutable reference to the task
    ///
    /// # Returns
    ///
    /// * `true` if the task was found and updated, `false` if GID not found
    pub fn update_task<F>(&mut self, gid: &str, mut updater: F) -> bool
    where
        F: FnOnce(&mut TaskProgress),
    {
        if let Some(task) = self.tasks.iter_mut().find(|t| t.gid == gid) {
            updater(task);
            true
        } else {
            false
        }
    }

    /// Get the number of currently tracked tasks.
    pub fn task_count(&self) -> usize {
        self.tasks.len()
    }

    /// Check if it's time to render again (rate limiting).
    pub fn should_render(&self) -> bool {
        self.last_render.elapsed() >= self.render_interval
    }

    /// Render the full progress display to a string.
    ///
    /// Produces a multi-line string containing:
    /// - Per-task progress bars (one per tracked task)
    /// - An overall summary line at the bottom
    ///
    /// In quiet mode, returns an empty string.
    pub fn render(&self) -> String {
        if self.quiet {
            return String::new();
        }

        let mut output = String::new();

        // Render each task's progress bar
        for (i, task) in self.tasks.iter().enumerate() {
            output.push_str(&self.render_task_bar(task, i + 1));
            output.push('\n');
        }

        // Render overall summary if there are tasks
        if !self.tasks.is_empty() {
            output.push_str(&self.render_overall_summary());
        }

        output
    }

    /// Render a single task's progress bar lines.
    ///
    /// Produces 2-3 lines depending on task type:
    /// - Line 1: Task header with index and filename
    /// - Line 2: Progress bar with stats
    /// - Line 3 (BT only): Seed/peer/upload info
    ///
    /// # Arguments
    ///
    /// * `task` - The task to render
    /// * `index` - 1-based display index for this task
    pub fn render_task_bar(&self, task: &TaskProgress, index: usize) -> String {
        let mut lines = Vec::new();

        // Header line: [#N] filename
        lines.push(format!("[#{}] {}", index, task.filename));

        // Determine what to show based on status
        match task.status {
            TaskStatus::Seeding => {
                // Seeding mode: show [SEEDING] instead of percentage
                let ratio = if task.total_length > 0 {
                    task.uploaded as f64 / task.total_length as f64
                } else {
                    0.0
                };
                let bar = format_progress_bar(1.0, self.width);
                lines.push(format!(
                    "     {} [SEEDING]  ({}/{})  UL:{}  Ratio:{:.2}",
                    bar,
                    format_bytes(task.completed_length),
                    format_bytes(task.total_length),
                    format_speed(task.upload_speed),
                    ratio
                ));
            }
            TaskStatus::Complete => {
                let bar = format_progress_bar(1.0, self.width);
                lines.push(format!(
                    "     {} [COMPLETE]  ({}/{})",
                    bar,
                    format_bytes(task.completed_length),
                    format_bytes(task.total_length)
                ));
            }
            TaskStatus::Error => {
                let fraction = if task.total_length > 0 {
                    task.completed_length as f64 / task.total_length as f64
                } else {
                    0.0
                };
                let bar = format_progress_bar(fraction, self.width);
                lines.push(format!(
                    "     {} [ERROR]  ({}/{})",
                    bar,
                    format_bytes(task.completed_length),
                    format_bytes(task.total_length)
                ));
            }
            TaskStatus::Waiting => {
                let bar = format_progress_bar(0.0, self.width);
                lines.push(format!(
                    "     {} [WAITING]  ({}/{})",
                    bar,
                    format_bytes(0),
                    format_bytes(task.total_length)
                ));
            }
            TaskStatus::Active => {
                let fraction = if task.total_length > 0 {
                    task.completed_length as f64 / task.total_length as f64
                } else {
                    0.0
                };
                let percentage = fraction * 100.0;
                let bar = format_progress_bar(fraction, self.width);

                let eta = format_eta(
                    task.total_length.saturating_sub(task.completed_length),
                    task.download_speed,
                );

                let eta_str = match eta {
                    Some(s) => format!("  ETA:{}", s),
                    None => String::new(),
                };

                lines.push(format!(
                    "     {} {:.1}%  ({}/{})  DL:{}{}",
                    bar,
                    percentage,
                    format_bytes(task.completed_length),
                    format_bytes(task.total_length),
                    format_speed(task.download_speed),
                    eta_str
                ));

                // BT extra info line
                if task.is_bt {
                    let ratio = if task.total_length > 0 {
                        task.uploaded as f64 / task.total_length as f64
                    } else {
                        0.0
                    };
                    lines.push(format!(
                        "     (S:{} P:{} U:{} Ratio:{:.2})",
                        task.num_seeders,
                        task.num_peers,
                        format_bytes(task.uploaded),
                        ratio
                    ));
                }
            }
        }

        lines.join("\n")
    }

    /// Render the overall summary line showing aggregate statistics.
    ///
    /// Displays total progress across all active tasks combined,
    /// aggregate speeds, and active/total task counts.
    pub fn render_overall_summary(&self) -> String {
        if self.tasks.is_empty() {
            return String::new();
        }

        let total_length: u64 = self.tasks.iter().map(|t| t.total_length).sum();
        let completed_length: u64 = self.tasks.iter().map(|t| t.completed_length).sum();
        let total_download_speed: f64 = self.tasks.iter().map(|t| t.download_speed).sum();
        let active_count = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Active || t.status == TaskStatus::Seeding)
            .count();
        let total_count = self.tasks.len();

        let fraction = if total_length > 0 {
            completed_length as f64 / total_length as f64
        } else {
            0.0
        };
        let percentage = fraction * 100.0;
        let bar = format_progress_bar(fraction, self.width);

        format!(
            "Overall: {} {:.0}%  ({}/{})  DL:{}  {}/{} total\n",
            bar,
            percentage,
            format_bytes(completed_length),
            format_bytes(total_length),
            format_speed(total_download_speed),
            active_count,
            total_count
        )
    }

    /// Format a visual progress bar string.
    ///
    /// Uses filled blocks (`█`) for completed portion and light
    /// shaded blocks (`░`) for remaining portion.
    ///
    /// # Arguments
    ///
    /// * `fraction` - Completion ratio from 0.0 to 1.0
    /// * `width` - Total width of the bar in characters
    ///
    /// # Returns
    ///
    /// * String like `[████████░░░░░░░░]`
    pub fn format_progress_bar(fraction: f64, width: usize) -> String {
        format_progress_bar(fraction, width)
    }

    /// Get elapsed time since creation.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

// ==================== Helper Functions ====================

/// Format a byte count into human-readable form using binary prefixes (KiB/MiB/GiB).
///
/// # Arguments
///
/// * `bytes` - Raw byte count
///
/// # Returns
///
/// * Formatted string like `"12.3 MiB"` or `"456 KiB"`
pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;

    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Format a transfer speed into human-readable form.
///
/// # Arguments
///
/// * `speed` - Speed in bytes per second
///
/// # Returns
///
/// * Formatted string like `"2.34 MiB/s"` or `"512 KiB/s"`
pub fn format_speed(speed: f64) -> String {
    if speed < 1024.0 {
        format!("{:.2} B/s", speed)
    } else if speed < 1024.0 * 1024.0 {
        format!("{:.2} KiB/s", speed / 1024.0)
    } else if speed < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} MiB/s", speed / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GiB/s", speed / (1024.0 * 1024.0 * 1024.0))
    }
}

/// Calculate and format estimated time arrival (ETA).
///
/// # Arguments
///
/// * `total_remaining` - Bytes still to download
/// * `speed` - Current download speed in bytes/second
///
/// # Returns
///
/// * `Some(String)` like `"3m12s"` if speed > 0
/// * `None` if speed is zero or near-zero (cannot estimate)
pub fn format_eta(total_remaining: u64, speed: f64) -> Option<String> {
    if speed <= 0.0 || total_remaining == 0 {
        return None;
    }

    let secs = total_remaining as f64 / speed;
    let duration = Duration::from_secs_f64(secs);

    let total_secs = duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;

    if hours > 0 {
        Some(format!("{}h{}m{}s", hours, minutes, seconds))
    } else if minutes > 0 {
        Some(format!("{}m{}s", minutes, seconds))
    } else {
        Some(format!("{}s", seconds))
    }
}

/// Internal function to render a progress bar character string.
///
/// Produces a bar like `[████████░░░░░░░░]`.
fn format_progress_bar(fraction: f64, width: usize) -> String {
    let clamped = fraction.max(0.0).min(1.0);
    let filled = (clamped * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);

    let bar: String = "█".repeat(filled) + &"░".repeat(empty);
    format!("[{}]", bar)
}

// ==================== Tests ====================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_active_task() -> TaskProgress {
        TaskProgress {
            gid: "abc123".to_string(),
            filename: "test-file.iso".to_string(),
            total_length: 100 * 1024 * 1024,        // 100 MiB
            completed_length: 65 * 1024 * 1024,     // 65 MiB
            download_speed: 2.34 * 1024.0 * 1024.0, // 2.34 MiB/s
            upload_speed: 0.5 * 1024.0 * 1024.0,
            is_bt: false,
            num_seeders: 0,
            num_peers: 0,
            uploaded: 0,
            status: TaskStatus::Active,
        }
    }

    fn make_bt_task() -> TaskProgress {
        TaskProgress {
            gid: "bt456".to_string(),
            filename: "ubuntu-22.04-desktop-amd64.iso".to_string(),
            total_length: 4700 * 1024 * 1024, // ~4.4 GiB
            completed_length: 3000 * 1024 * 1024,
            download_speed: 5.6 * 1024.0 * 1024.0,
            upload_speed: 1.2 * 1024.0 * 1024.0,
            is_bt: true,
            num_seeders: 3,
            num_peers: 12,
            uploaded: 1700 * 1024 * 1024,
            status: TaskStatus::Active,
        }
    }

    #[test]
    fn test_single_task_render() {
        let mut bar = ProgressBar::new(false);
        let task = make_active_task();
        bar.add_task(task);

        let output = bar.render();

        // Verify key components are present
        assert!(output.contains("[#1]"), "Should have task header");
        assert!(output.contains("test-file.iso"), "Should have filename");
        assert!(
            output.contains("65.0%") || output.contains("65."),
            "Should show percentage"
        );
        assert!(output.contains("MiB"), "Should use MiB units");
        assert!(output.contains("DL:"), "Should show download speed label");
        assert!(output.contains("ETA:"), "Should show ETA");
    }

    #[test]
    fn test_multi_task_render() {
        let mut bar = ProgressBar::new(false);

        // Add 3 tasks with different statuses
        let mut task1 = make_active_task();
        task1.gid = "task1".to_string();
        task1.filename = "file1.bin".to_string();
        bar.add_task(task1);

        let mut task2 = make_active_task();
        task2.gid = "task2".to_string();
        task2.filename = "file2.iso".to_string();
        task2.status = TaskStatus::Waiting;
        bar.add_task(task2);

        let mut task3 = make_active_task();
        task3.gid = "task3".to_string();
        task3.filename = "file3.dat".to_string();
        task3.status = TaskStatus::Complete;
        task3.completed_length = task3.total_length;
        bar.add_task(task3);

        let output = bar.render();

        // Verify overall summary present
        assert!(output.contains("Overall:"), "Should have overall summary");

        // Verify individual task headers
        assert!(output.contains("[#1]"), "Should have task #1");
        assert!(output.contains("[#2]"), "Should have task #2");
        assert!(output.contains("[#3]"), "Should have task #3");

        // Verify different statuses rendered
        assert!(
            output.contains("ACTIVE") || output.contains("WAITING") || output.contains("COMPLETE")
        );

        // Verify task count in summary
        assert!(output.contains("/3 total"), "Should show 3 total tasks");
    }

    #[test]
    fn test_quiet_mode() {
        let mut bar = ProgressBar::new(true); // Quiet mode ON
        bar.add_task(make_active_task());

        let output = bar.render();

        assert!(output.is_empty(), "Quiet mode should produce empty string");
    }

    #[test]
    fn test_seeding_display() {
        let mut bar = ProgressBar::new(false);

        let mut seeding_task = make_bt_task();
        seeding_task.status = TaskStatus::Seeding;
        seeding_task.completed_length = seeding_task.total_length;
        seeding_task.uploaded = 2000 * 1024 * 1024; // 2 GiB uploaded
        bar.add_task(seeding_task);

        let output = bar.render();

        assert!(
            output.contains("[SEEDING]"),
            "Seeding task should show SEEDING tag"
        );
        // Note: The overall summary may contain %, so we check that the seeding
        // task line shows [SEEDING] tag rather than a percentage like "65.2%"
        assert!(
            output.contains("Ratio:"),
            "Seeding task should show upload ratio"
        );
        assert!(
            output.contains("UL:"),
            "Seeding task should show upload speed"
        );
    }

    #[test]
    fn test_bt_extra_fields() {
        let mut bar = ProgressBar::new(false);
        let bt_task = make_bt_task();
        bar.add_task(bt_task);

        let output = bar.render();

        // BT-specific fields should be present
        assert!(output.contains("(S:"), "Should show seeder count");
        assert!(output.contains("P:"), "Should show peer count");
        assert!(output.contains("U:"), "Should show uploaded bytes");
        assert!(output.contains("Ratio:"), "Should show share ratio");

        // Verify actual values
        assert!(output.contains("S:3"), "Should show 3 seeders");
        assert!(output.contains("P:12"), "Should show 12 peers");
    }

    #[test]
    fn test_format_bytes_units() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(2048), "2.00 KiB");
        assert_eq!(format_bytes(1048576), "1.00 MiB");
        assert_eq!(format_bytes(1073741824), "1.00 GiB");
    }

    #[test]
    fn test_format_speed_units() {
        let result = format_speed(512.0);
        assert!(result.contains("B/s"));

        let result = format_speed(2048.0);
        assert!(result.contains("KiB/s"));

        let result = format_speed(2.0 * 1024.0 * 1024.0);
        assert!(result.contains("MiB/s"));
    }

    #[test]
    fn test_format_eta_calculation() {
        // 10 MB remaining at 1 MB/s = 10 seconds
        let eta = format_eta(10 * 1024 * 1024, 1024.0 * 1024.0);
        assert!(eta.is_some());
        assert!(eta.unwrap().contains("10s"));

        // Zero speed should return None
        assert!(format_eta(1000, 0.0).is_none());

        // Nothing remaining should return None
        assert!(format_eta(0, 1024.0).is_none());
    }

    #[test]
    fn test_format_progress_bar_visual() {
        let bar_0 = format_progress_bar(0.0, 10);
        assert!(bar_0.contains('░'), "Empty bar should have empty chars");
        assert!(
            !bar_0.contains('█'),
            "Empty bar should not have filled chars"
        );

        let bar_full = format_progress_bar(1.0, 10);
        assert!(bar_full.contains('█'), "Full bar should have filled chars");
        assert!(
            !bar_full.contains('░'),
            "Full bar should not have empty chars"
        );

        let bar_half = format_progress_bar(0.5, 10);
        assert!(
            bar_half.contains('█'),
            "Half bar should have some filled chars"
        );
        assert!(
            bar_half.contains('░'),
            "Half bar should have some empty chars"
        );
    }

    #[test]
    fn test_add_remove_tasks() {
        let mut bar = ProgressBar::new(false);

        assert_eq!(bar.task_count(), 0);

        bar.add_task(make_active_task());
        assert_eq!(bar.task_count(), 1);

        bar.add_task(make_bt_task());
        assert_eq!(bar.task_count(), 2);

        assert!(bar.remove_task("abc123"));
        assert_eq!(bar.task_count(), 1);

        assert!(!bar.remove_task("nonexistent")); // Already removed
        assert_eq!(bar.task_count(), 1);
    }

    #[test]
    fn test_update_task() {
        let mut bar = ProgressBar::new(false);
        bar.add_task(make_active_task());

        let updated = bar.update_task("abc123", |task| {
            task.completed_length = 80 * 1024 * 1024; // Update to 80%
            task.download_speed = 5.0 * 1024.0 * 1024.0;
        });

        assert!(updated, "Update should succeed for existing GID");

        let no_update = bar.update_task("nonexistent", |_task| {});
        assert!(!no_update, "Update should fail for non-existent GID");
    }

    #[test]
    fn test_should_render_rate_limiting() {
        let bar = ProgressBar::new(false);

        // First call should always be allowed (last_render initialized in past)
        assert!(bar.should_render());
    }

    #[test]
    fn test_complete_task_display() {
        let mut bar = ProgressBar::new(false);

        let mut task = make_active_task();
        task.status = TaskStatus::Complete;
        task.completed_length = task.total_length;
        bar.add_task(task);

        let output = bar.render();
        assert!(output.contains("[COMPLETE]"));
    }

    #[test]
    fn test_error_task_display() {
        let mut bar = ProgressBar::new(false);

        let mut task = make_active_task();
        task.status = TaskStatus::Error;
        bar.add_task(task);

        let output = bar.render();
        assert!(output.contains("[ERROR]"));
    }
}
