//! Console Progress Reporter — periodic download progress output
//!
//! Polls `RequestGroupMan` at a fixed interval and renders active download
//! progress to stdout using the `ProgressBar` UI component. Designed to run
//! as a background `tokio` task alongside the download engine.
//!
//! # Display
//!
//! Overwrites previous output using ANSI escape codes for smooth in-place
//! updates on each tick:
//!
//! ```text
//! [#1] file.iso
//!      [████████████░░░░░░░] 65.2%  (12.3MiB / 18.9MiB)  DL:2.34MiB/s  ETA:3m12s
//! Overall: [████████░░░░░░░░░░] 42%  (450MiB / 1.07GiB)  DL:5.67MiB/s  3 active / 8 total
//! ```

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::sync::RwLock;
use tokio::time::sleep;

use crate::ui::progress_bar::{ProgressBar, TaskProgress, TaskStatus};
use aria2_core::request::request_group::DownloadStatus;
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_core::util::rwlock_ext::RwLockRecover;

/// Console progress reporter that periodically polls `RequestGroupMan`
/// and renders active download progress to stdout.
pub struct ConsoleProgressReporter {
    group_man: Arc<RwLock<RequestGroupMan>>,
    interval: Duration,
    /// Signal receiver – when the sender is dropped/sent, the loop exits.
    stop_rx: Option<oneshot::Receiver<()>>,
    /// Number of lines printed in the last render (for cursor-up overwriting)
    last_line_count: usize,
    /// Whether we've rendered at least once
    has_rendered: bool,
}

impl ConsoleProgressReporter {
    /// Create a new progress reporter.
    ///
    /// Returns the reporter and a `oneshot::Sender` that signals the reporter
    /// to stop (on drop or explicit send).
    ///
    /// # Arguments
    ///
    /// * `group_man` - Shared request group manager
    pub fn new(group_man: Arc<RwLock<RequestGroupMan>>) -> (Self, oneshot::Sender<()>) {
        let (stop_tx, stop_rx) = oneshot::channel();
        let reporter = Self {
            group_man,
            interval: Duration::from_millis(crate::constants::PROGRESS_BAR_RENDER_INTERVAL_MS),
            stop_rx: Some(stop_rx),
            last_line_count: 0,
            has_rendered: false,
        };
        (reporter, stop_tx)
    }

    /// Run the reporting loop until the stop signal is received.
    ///
    /// Polls `RequestGroupMan` at the configured interval and renders progress
    /// to stdout. On stop, moves the cursor past the last output block.
    pub async fn run(&mut self) {
        let stop_rx = self
            .stop_rx
            .take()
            .expect("ConsoleProgressReporter::run called twice");
        // Pin the receiver so we can poll it with `&mut` in the select loop.
        tokio::pin!(stop_rx);

        loop {
            tokio::select! {
                biased;
                _ = &mut stop_rx => {
                    // Channel closed or signal sent -> exit.
                    if self.last_line_count > 0 {
                        // Move past the rendered block so the shell prompt
                        // appears cleanly below the progress output.
                        println!();
                    }
                    break;
                }
                _ = sleep(self.interval) => {
                    self.tick().await;
                }
            }
        }
    }

    /// Single poll-render cycle.
    async fn tick(&mut self) {
        let man = self.group_man.read().await;
        let all_groups = man.all_groups();
        drop(man);

        // Build TaskProgress list from active/waiting groups.
        let mut tasks: Vec<TaskProgress> = Vec::new();
        for (gid, group_lock) in &all_groups {
            let group = group_lock.recover();
            let status = group.status();

            let task_status = match &status {
                DownloadStatus::Active => TaskStatus::Active,
                DownloadStatus::Waiting => TaskStatus::Waiting,
                DownloadStatus::Complete => continue,
                DownloadStatus::Error(_) => continue,
                DownloadStatus::Paused => TaskStatus::Waiting,
                DownloadStatus::Removed => continue,
            };

            let completed = group.get_completed_length();
            let total = group.get_total_length_atomic();
            let speed = group.get_download_speed_cached();

            let filename = group
                .uris()
                .first()
                .map(|u| extract_filename(u))
                .unwrap_or_else(|| format!("gid#{}", gid.to_hex_string()));

            tasks.push(TaskProgress {
                gid: gid.to_hex_string(),
                filename,
                total_length: total,
                completed_length: completed,
                download_speed: speed as f64,
                upload_speed: 0.0,
                is_bt: false,
                num_seeders: 0,
                num_peers: 0,
                uploaded: 0,
                status: task_status,
            });
        }

        // Nothing to show: clear any stale output and return.
        if tasks.is_empty() {
            if self.last_line_count > 0 {
                self.clear_previous_output();
                self.last_line_count = 0;
                self.has_rendered = false;
            }
            return;
        }

        // Render with a fresh ProgressBar each tick.
        let mut bar = ProgressBar::new(false);
        for task in &tasks {
            bar.add_task(task.clone());
        }
        let output = bar.render();
        let line_count = output.lines().count();

        // Overwrite previous output block.
        if self.has_rendered && self.last_line_count > 0 {
            // Move cursor up by last_line_count lines, then clear everything
            // from cursor to end of screen before writing new content.
            print!("\x1b[{}A\x1b[J", self.last_line_count);
        }
        print!("{}", output);

        self.last_line_count = line_count;
        self.has_rendered = true;
    }

    /// Clear the previously rendered output block from the terminal.
    fn clear_previous_output(&self) {
        if self.has_rendered && self.last_line_count > 0 {
            print!("\x1b[{}A\x1b[J", self.last_line_count);
        }
    }
}

/// Extract a human-readable filename from a URI.
///
/// Takes the last path segment after `/`. Returns `"unknown"` if the URI
/// is empty or ends with `/`.
fn extract_filename(uri: &str) -> String {
    uri.rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_filename_http() {
        assert_eq!(
            extract_filename("http://example.com/file.iso"),
            "file.iso"
        );
    }

    #[test]
    fn test_extract_filename_path() {
        assert_eq!(extract_filename("/path/to/file.txt"), "file.txt");
    }

    #[test]
    fn test_extract_filename_empty() {
        assert_eq!(extract_filename("http://example.com/"), "unknown");
    }

    #[test]
    fn test_extract_filename_no_path() {
        // "http://example.com" yields "example.com" (the host)
        assert_eq!(extract_filename("http://example.com"), "example.com");
    }
}
