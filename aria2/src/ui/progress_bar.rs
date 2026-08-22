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

use aria2_core::request::request_group::DownloadResult;

/// Status of a single download task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    /// Task is actively downloading/uploading
    Active,
    /// Task is queued and waiting to start
    Waiting,
    /// Task is paused by the user
    Paused,
    /// Download completed successfully
    Complete,
    /// Task encountered an error
    Error,
    /// Task was explicitly removed by the user
    Removed,
    /// Task is in seeding mode (BT only)
    Seeding,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskStatus::Active => write!(f, "{}", crate::constants::STATUS_ACTIVE),
            TaskStatus::Waiting => write!(f, "{}", crate::constants::STATUS_WAITING),
            TaskStatus::Paused => write!(f, "{}", crate::constants::STATUS_PAUSED),
            TaskStatus::Complete => write!(f, "{}", crate::constants::STATUS_COMPLETE),
            TaskStatus::Error => write!(f, "{}", crate::constants::STATUS_ERROR),
            TaskStatus::Removed => write!(f, "{}", crate::constants::STATUS_REMOVED),
            TaskStatus::Seeding => write!(f, "{}", crate::constants::STATUS_SEEDING),
        }
    }
}

/// Progress data for a single download task.
///
/// Contains all fields needed to render the task's progress bar line(s).
#[derive(Clone)]
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
    /// Time spent downloading this task.
    pub elapsed: Duration,
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
    /// Optional terminal width used to keep task headers within one line.
    max_line_width: Option<usize>,
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
            width: crate::constants::PROGRESS_BAR_WIDTH, // Default bar width in characters
            tasks: Vec::new(),
            started: Instant::now(),
            last_render: Instant::now() - Duration::from_secs(1), // Allow immediate first render
            render_interval: Duration::from_millis(250),          // ~4 FPS max
            max_line_width: None,
        }
    }

    /// Set the width of the progress bar (in characters).
    ///
    /// Default is 24 characters. Minimum effective value is 4.
    pub fn with_width(mut self, width: usize) -> Self {
        self.width = width.max(crate::constants::PROGRESS_BAR_MIN_WIDTH);
        self
    }

    /// Set the minimum interval between render calls.
    ///
    /// Default is 250ms (~4 FPS). Used for rate-limiting terminal updates.
    pub fn with_render_interval(mut self, interval: Duration) -> Self {
        self.render_interval = interval;
        self
    }

    /// Set the available terminal width for line-local rendering.
    pub fn with_terminal_width(mut self, width: usize) -> Self {
        self.max_line_width = Some(width.max(1));
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
    pub fn update_task<F>(&mut self, gid: &str, updater: F) -> bool
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
        self.render_with_summary(true)
    }

    /// Render the task readout, optionally including the aggregate summary.
    ///
    /// `summary-interval=0` disables periodic aggregate summaries while
    /// preserving per-task progress output. The final application summary is
    /// rendered separately and is not affected by this flag.
    pub fn render_with_summary(&self, include_summary: bool) -> String {
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
        if include_summary && !self.tasks.is_empty() {
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
        let filename = self
            .max_line_width
            .map(|width| truncate_display(&task.filename, width.saturating_sub(5)))
            .unwrap_or_else(|| task.filename.clone());
        lines.push(format!("[#{}] {}", index, filename));

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
                    "     {} [COMPLETE]  ({}/{})  Avg:{}  Time:{}",
                    bar,
                    format_bytes(task.completed_length),
                    format_bytes(task.total_length),
                    format_speed(average_speed(task)),
                    aria2_core::util::format::format_duration_short(task.elapsed.as_secs())
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
            TaskStatus::Removed => {
                let fraction = progress_fraction(task);
                let bar = format_progress_bar(fraction, self.width);
                lines.push(format!(
                    "     {} [REMOVED]  ({}/{})",
                    bar,
                    format_bytes(task.completed_length),
                    format_bytes(task.total_length)
                ));
            }
            TaskStatus::Paused => {
                let fraction = progress_fraction(task);
                let bar = format_progress_bar(fraction, self.width);
                lines.push(format!(
                    "     {} [PAUSED]  ({}/{})",
                    bar,
                    format_bytes(task.completed_length),
                    format_bytes(task.total_length)
                ));
            }
            TaskStatus::Active => {
                let fraction = progress_fraction(task);
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
                    "     {} {}  ({}/{})  DL:{}{}  Time:{}",
                    bar,
                    format_percentage(task),
                    format_bytes(task.completed_length),
                    format_bytes(task.total_length),
                    format_speed(task.download_speed),
                    eta_str,
                    aria2_core::util::format::format_duration_short(task.elapsed.as_secs())
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
        let total_download_speed: f64 = self
            .tasks
            .iter()
            .map(|task| match task.status {
                TaskStatus::Complete => average_speed(task),
                _ if task.download_speed > 0.0 => task.download_speed,
                _ => average_speed(task),
            })
            .sum();
        let total_elapsed = self
            .tasks
            .iter()
            .map(|task| task.elapsed)
            .max()
            .unwrap_or_default();
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
        let percentage = if total_length > 0 {
            format!("{:.0}%", fraction * 100.0)
        } else {
            "--%".to_string()
        };
        let bar = format_progress_bar(fraction, self.width);

        format!(
            "Overall: {} {} ({}/{}) DL:{} Time:{} {}/{} total\n",
            bar,
            percentage,
            format_bytes(completed_length),
            format_bytes(total_length),
            format_speed(total_download_speed),
            aria2_core::util::format::format_duration_short(total_elapsed.as_secs()),
            active_count,
            total_count
        )
    }

    /// Render one stable line for redirected output such as a PowerShell pipe.
    pub fn render_compact_summary(&self) -> String {
        self.render_overall_summary().trim_end().to_string()
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

// Re-export shared formatting functions from aria2-core
pub use aria2_core::util::format::{format_bytes, format_speed};

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

    let secs = (total_remaining as f64 / speed) as u64;
    Some(aria2_core::util::format::format_duration_short(secs))
}

/// Internal function to render a progress bar character string.
///
/// Produces a bar like `[████████░░░░░░░░]`.
fn format_progress_bar(fraction: f64, width: usize) -> String {
    let clamped = fraction.clamp(0.0, 1.0);
    let filled = (clamped * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);

    let bar: String = "█".repeat(filled) + &"░".repeat(empty);
    format!("[{}]", bar)
}

fn truncate_display(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    if max_chars <= 3 {
        return value.chars().take(max_chars).collect();
    }
    let prefix: String = value.chars().take(max_chars - 3).collect();
    format!("{prefix}...")
}

/// Render stable per-download results and aggregate statistics after the
/// engine exits. This output is intentionally independent of the live TTY
/// frame so a completed result remains readable in logs and pipes.
pub fn render_final_summary(results: &[DownloadResult]) -> String {
    if results.is_empty() {
        return String::new();
    }

    let completed = results
        .iter()
        .filter(|result| result.code.is_success())
        .count();
    let failed = results.len().saturating_sub(completed);
    let total_bytes = results
        .iter()
        .map(|result| result.completed_length)
        .sum::<u64>();
    let total_time = results
        .iter()
        .map(|result| result.session_time)
        .max()
        .unwrap_or_default();
    let average_speed = if total_time > 0 {
        total_bytes as f64 / total_time as f64
    } else {
        results
            .iter()
            .map(|result| result.download_speed as f64)
            .sum()
    };

    let mut output = String::from("Download results:\n");
    for result in results {
        let filename = result
            .files
            .first()
            .map(|file| file.path.as_str())
            .filter(|path| !path.is_empty())
            .unwrap_or("unknown");
        let elapsed = aria2_core::util::format::format_duration_short(result.session_time);
        if result.code.is_success() {
            let speed = if result.session_time > 0 {
                result.completed_length as f64 / result.session_time as f64
            } else {
                result.download_speed as f64
            };
            output.push_str(&format!(
                "[#{}] COMPLETE {} Size:{}/{} Avg:{} Time:{}\n",
                result.gid_hex(),
                filename,
                format_bytes(result.completed_length),
                format_bytes(result.total_length),
                format_speed(speed),
                elapsed
            ));
        } else {
            output.push_str(&format!(
                "[#{}] ERROR {} Size:{}/{} Code:{} Time:{} Message:{}\n",
                result.gid_hex(),
                filename,
                format_bytes(result.completed_length),
                format_bytes(result.total_length),
                result.code,
                elapsed,
                result.message
            ));
        }
    }
    output.push_str(&format!(
        "Overall: {} tasks, {} complete, {} failed, Total:{}, Time:{}, Avg:{}\n",
        results.len(),
        completed,
        failed,
        format_bytes(total_bytes),
        aria2_core::util::format::format_duration_short(total_time),
        format_speed(average_speed)
    ));
    output
}

fn average_speed(task: &TaskProgress) -> f64 {
    let seconds = task.elapsed.as_secs_f64();
    if seconds > 0.0 {
        task.completed_length as f64 / seconds
    } else {
        task.download_speed
    }
}

fn progress_fraction(task: &TaskProgress) -> f64 {
    if task.total_length == 0 {
        0.0
    } else {
        (task.completed_length as f64 / task.total_length as f64).clamp(0.0, 1.0)
    }
}

fn format_percentage(task: &TaskProgress) -> String {
    if task.total_length == 0 {
        "--%".to_string()
    } else {
        format!("{:.1}%", progress_fraction(task) * 100.0)
    }
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
            elapsed: Duration::from_secs(31),
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
            elapsed: Duration::from_secs(120),
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
        assert!(output.contains("Time:31s"), "Should show elapsed time");
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
        assert!(output.contains("Avg:"));
        assert!(output.contains("Time:31s"));
    }

    #[test]
    fn test_complete_task_uses_average_speed_when_current_speed_is_zero() {
        let mut bar = ProgressBar::new(false);
        let mut task = make_active_task();
        task.status = TaskStatus::Complete;
        task.completed_length = 100;
        task.total_length = 100;
        task.download_speed = 0.0;
        task.elapsed = Duration::from_secs(10);
        bar.add_task(task);

        let output = bar.render();
        assert!(output.contains("Avg:10 B/s"));
    }

    #[test]
    fn test_compact_summary_contains_completion_statistics() {
        let mut bar = ProgressBar::new(false);
        let mut task = make_active_task();
        task.status = TaskStatus::Complete;
        task.completed_length = task.total_length;
        bar.add_task(task);

        let output = bar.render_compact_summary();
        assert!(output.contains("Overall:"));
        assert!(output.contains("DL:"));
        assert!(output.contains("Time:31s"));
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

    #[test]
    fn test_paused_task_display_is_distinct_from_waiting() {
        let mut bar = ProgressBar::new(false);
        let mut task = make_active_task();
        task.status = TaskStatus::Paused;
        task.completed_length = 25 * 1024 * 1024;
        bar.add_task(task);

        let output = bar.render();
        assert!(output.contains("[PAUSED]"));
        assert!(!output.contains("[WAITING]"));
    }

    #[test]
    fn test_removed_task_display_is_distinct_from_error() {
        let mut bar = ProgressBar::new(false);
        let mut task = make_active_task();
        task.status = TaskStatus::Removed;
        bar.add_task(task);

        let output = bar.render();
        assert!(output.contains("[REMOVED]"));
        assert!(!output.contains("[ERROR]"));
    }

    #[test]
    fn test_long_filename_is_truncated_to_terminal_width() {
        let mut bar = ProgressBar::new(false)
            .with_width(4)
            .with_terminal_width(32);
        let mut task = make_active_task();
        task.filename = "a-very-long-file-name-that-would-wrap.bin".to_string();
        bar.add_task(task);

        let output = bar.render();
        let header = output.lines().next().unwrap();
        assert!(header.chars().count() <= 32);
        assert!(header.ends_with("..."));
    }

    #[test]
    fn test_unknown_total_does_not_report_zero_percent() {
        let mut bar = ProgressBar::new(false);
        let mut task = make_active_task();
        task.total_length = 0;
        task.completed_length = 0;
        bar.add_task(task);

        let output = bar.render();
        assert!(output.contains("--%"));
        assert!(!output.contains("0.0%"));
    }

    #[test]
    fn test_final_summary_contains_terminal_statistics() {
        let mut result = DownloadResult::finished();
        result.completed_length = 2048;
        result.total_length = 2048;
        result.session_time = 2;

        let output = render_final_summary(&[result]);
        assert!(output.contains("Download results:"));
        assert!(output.contains("1 complete, 0 failed"));
        assert!(output.contains("Avg:1.00 KiB/s"));
    }

    #[test]
    fn test_render_performance_snapshot() {
        for task_count in [1usize, 8, 64] {
            let mut bar = ProgressBar::new(false).with_width(16);
            for index in 0..task_count {
                let mut task = make_active_task();
                task.gid = format!("task-{index}");
                task.filename = format!("file-{index:03}.bin");
                bar.add_task(task);
            }

            let mut samples = Vec::with_capacity(100);
            let mut bytes_per_frame = 0;
            for _ in 0..100 {
                let start = Instant::now();
                let output = bar.render();
                bytes_per_frame = output.len();
                std::hint::black_box(output);
                samples.push(start.elapsed().as_nanos() as u64);
            }
            samples.sort_unstable();
            let average_ns = samples.iter().sum::<u64>() / samples.len() as u64;
            let p95_ns = samples[(samples.len() * 95 / 100).saturating_sub(1)];
            println!(
                "ui-perf tasks={task_count} frames=100 avg_ns={average_ns} p95_ns={p95_ns} bytes_per_frame={bytes_per_frame}"
            );
        }
    }
}
