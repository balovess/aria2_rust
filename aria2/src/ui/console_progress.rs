//! Console Progress Reporter — event-driven download progress output
//!
//! Waits for `RequestGroupMan` activity and renders active download progress
//! to stdout using the `ProgressBar` UI component. Rendering is throttled by a
//! deadline, but an idle reporter does not wake up on a fixed interval.
//!
//! # Display
//!
//! Overwrites previous output using ANSI escape codes for smooth in-place
//! updates on each tick:
//!
//! ```text
//! [1] file.iso 65% [████████████░░░░░░░] 12.3 MiB/18.9 MiB 2.34 MiB/s ETA 3m12s
//! Total 8 tasks 42% [████████░░░░░░░░░░] 5.67 MiB/s active 3 ETA 3m12s
//! ```

use std::collections::HashSet;
use std::io::IsTerminal;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio::time::sleep_until;

use crate::ui::progress_bar::{ProgressBar, TaskProgress, TaskStatus};
use aria2_core::request::request_group::DownloadStatus;
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_core::util::rwlock_ext::RwLockRecover;

/// Console progress reporter that renders active download progress after
/// manager activity, with a bounded render rate.
pub struct ConsoleProgressReporter {
    group_man: Arc<RequestGroupMan>,
    interval: Duration,
    /// Whether stdout is a terminal. Non-terminal output must stay line based
    /// for consumers such as Scoop's PowerShell pipeline.
    terminal_output: bool,
    /// Signal receiver – when the sender is dropped/sent, the loop exits.
    stop_rx: Option<oneshot::Receiver<()>>,
    /// Number of lines printed in the last render (for cursor-up overwriting)
    last_line_count: usize,
    /// Whether we've rendered at least once
    has_rendered: bool,
    /// Stopped results that existed before this reporter started.
    known_stopped: HashSet<String>,
    /// Terminal results observed during this reporter run. Keep these
    /// snapshots visible until the reporter exits so the final statistics do
    /// not disappear on the next refresh.
    terminal_results: Vec<aria2_core::request::request_group::DownloadResult>,
    /// Interval for aggregate progress summaries. `None` means disabled.
    summary_interval: Option<Duration>,
    last_summary_at: Option<Instant>,
    output_to_stderr: bool,
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
    pub fn new(group_man: Arc<RequestGroupMan>) -> (Self, oneshot::Sender<()>) {
        Self::new_with_options(group_man, 60, false)
    }

    /// Create a reporter with the configured summary interval in seconds.
    ///
    /// The readout itself remains rate-limited to avoid excessive terminal
    /// writes. The interval controls only the aggregate summary line, matching
    /// aria2's distinction between console readout and summary output.
    pub fn new_with_options(
        group_man: Arc<RequestGroupMan>,
        summary_interval_secs: i64,
        output_to_stderr: bool,
    ) -> (Self, oneshot::Sender<()>) {
        let (stop_tx, stop_rx) = oneshot::channel();
        let reporter = Self {
            group_man: Arc::clone(&group_man),
            interval: Duration::from_millis(crate::constants::PROGRESS_BAR_RENDER_INTERVAL_MS),
            terminal_output: if output_to_stderr {
                io::stderr().is_terminal()
            } else {
                io::stdout().is_terminal()
            },
            stop_rx: Some(stop_rx),
            last_line_count: 0,
            has_rendered: false,
            known_stopped: group_man
                .get_stopped_results(0, usize::MAX)
                .into_iter()
                .map(|result| result.gid_hex())
                .collect(),
            terminal_results: Vec::new(),
            summary_interval: (summary_interval_secs > 0)
                .then_some(Duration::from_secs(summary_interval_secs as u64)),
            last_summary_at: None,
            output_to_stderr,
        };
        (reporter, stop_tx)
    }

    /// Run the reporting loop until the stop signal is received.
    ///
    /// The initial snapshot is rendered immediately. Later snapshots are
    /// driven by activity events and delayed only when the render throttle is
    /// active. On stop, moves the cursor past the last output block.
    pub async fn run(&mut self) {
        let stop_rx = self
            .stop_rx
            .take()
            .expect("ConsoleProgressReporter::run called twice");
        tokio::pin!(stop_rx);
        let activity = self.group_man.activity_signal();
        let mut observed_generation = activity.generation();
        let initial_rendered = self.tick(true).await;
        let mut last_render_at = initial_rendered.then(Instant::now);
        let mut render_deadline = None;

        loop {
            if let Some(deadline) = render_deadline.take() {
                let deadline_wait = sleep_until(tokio::time::Instant::from_std(deadline));
                tokio::pin!(deadline_wait);
                tokio::select! {
                    biased;
                    _ = &mut stop_rx => break,
                    _ = &mut deadline_wait => {
                        let rendered = self.tick(false).await;
                        last_render_at = rendered.then(Instant::now);
                    }
                }
                continue;
            }

            let activity_wait = activity.wait_for_change(&mut observed_generation);
            tokio::pin!(activity_wait);
            tokio::select! {
                biased;
                _ = &mut stop_rx => break,
                _ = &mut activity_wait => {
                    let now = Instant::now();
                    let can_render = last_render_at
                        .map(|last| now.duration_since(last) >= self.interval)
                        .unwrap_or(true);
                    if can_render {
                        let rendered = self.tick(false).await;
                        last_render_at = rendered.then(Instant::now);
                    } else if let Some(last) = last_render_at {
                        render_deadline = Some(last + self.interval);
                    }
                }
            }
        }

        if self.last_line_count > 0 {
            let rendered = self.tick(true).await;
            if rendered && self.terminal_output {
                self.write_stdout("\n");
            }
        }
    }

    /// Render one current snapshot.
    async fn tick(&mut self, force_summary: bool) -> bool {
        let all_groups = self.group_man.all_groups();
        let live_gids: HashSet<String> = all_groups
            .iter()
            .map(|(gid, _)| gid.to_hex_string())
            .collect();

        // Build TaskProgress list from active/waiting groups.
        let mut tasks: Vec<TaskProgress> = Vec::new();
        for (gid, group_lock) in &all_groups {
            let group = group_lock.recover();
            let status = group.status();

            let task_status = match &status {
                DownloadStatus::Active => TaskStatus::Active,
                DownloadStatus::Waiting => TaskStatus::Waiting,
                DownloadStatus::Complete => TaskStatus::Complete,
                DownloadStatus::Error(_) => TaskStatus::Error,
                DownloadStatus::Paused => TaskStatus::Paused,
                DownloadStatus::Removed => TaskStatus::Removed,
            };

            let snapshot = group.status_snapshot();
            let completed = snapshot.completed_length;
            let total = snapshot.total_length;
            let speed = snapshot.download_speed;
            let upload_speed = snapshot.upload_speed;
            let bt = snapshot.bt.as_ref();
            let is_bt = bt.is_some();
            let num_seeders = bt.map_or(0, |bt| bt.seeder_count());
            let num_peers = bt.map_or(0, |bt| bt.peer_count());

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
                upload_speed: upload_speed as f64,
                is_bt,
                connections: snapshot.connections as usize,
                num_seeders,
                num_peers,
                uploaded: snapshot.upload_length,
                status: task_status,
                elapsed: snapshot.elapsed.unwrap_or_default(),
            });
        }

        // Terminal groups are removed from `all_groups` as soon as the engine
        // demotes them. Include only results created during this run so an old
        // session does not reappear in a new progress display.
        for result in self.group_man.get_stopped_results(0, usize::MAX) {
            let gid = result.gid_hex();
            if live_gids.contains(&gid) {
                continue;
            }
            if !self.known_stopped.insert(gid.clone()) {
                continue;
            }
            self.terminal_results.push(result);
        }

        for result in &self.terminal_results {
            let gid = result.gid_hex();
            if live_gids.contains(&gid) {
                continue;
            }
            let status = if result.code.is_success() {
                TaskStatus::Complete
            } else if result.code.is_user_stopped() {
                TaskStatus::Removed
            } else {
                TaskStatus::Error
            };
            let filename = result
                .files
                .first()
                .map(|file| file.path.clone())
                .filter(|path| !path.is_empty())
                .unwrap_or_else(|| format!("gid#{}", gid));
            tasks.push(TaskProgress {
                gid,
                filename,
                total_length: result.total_length,
                completed_length: result.completed_length,
                download_speed: result.download_speed as f64,
                upload_speed: result.upload_speed as f64,
                is_bt: result.num_pieces > 0 || !result.info_hash.is_empty(),
                connections: 0,
                num_seeders: 0,
                num_peers: 0,
                uploaded: result.upload_length,
                status,
                elapsed: Duration::from_secs(result.session_time),
            });
        }

        // Nothing to show: clear any stale output and return.
        if tasks.is_empty() {
            if self.last_line_count > 0 {
                if self.terminal_output {
                    self.clear_previous_output();
                }
                self.last_line_count = 0;
                self.has_rendered = false;
            }
            return false;
        }

        // Render with a fresh ProgressBar each tick.
        let terminal_width = aria2_core::ui::terminal_width();
        let bar_width = terminal_width
            .saturating_sub(60)
            .clamp(crate::constants::PROGRESS_BAR_MIN_WIDTH, 24);
        let mut bar = ProgressBar::new(false)
            .with_width(bar_width)
            .with_terminal_width(terminal_width)
            .with_color(self.terminal_output);
        for task in &tasks {
            bar.add_task(task.clone());
        }
        let include_summary = force_summary
            || self
                .summary_interval
                .map(|interval| {
                    self.last_summary_at
                        .map(|last| last.elapsed() >= interval)
                        .unwrap_or(true)
                })
                .unwrap_or(false);
        if include_summary {
            self.last_summary_at = Some(Instant::now());
        }
        let output = if self.terminal_output {
            bar.render_with_summary(include_summary)
        } else {
            format!("{}\n", bar.render_with_summary(include_summary).trim_end())
        };
        let line_count = output.lines().count();

        // Overwrite terminal output in place. A redirected stream receives
        // complete plain-text frames and an explicit flush instead.
        let mut frame = String::new();
        if self.terminal_output && self.has_rendered && self.last_line_count > 0 {
            frame.push_str(&build_terminal_frame(self.last_line_count, &output));
        } else {
            frame.push_str(&output);
        }
        self.write_stdout(&frame);

        self.last_line_count = line_count;
        self.has_rendered = true;
        true
    }

    /// Clear the previously rendered output block from the terminal.
    fn clear_previous_output(&self) {
        if self.terminal_output && self.has_rendered && self.last_line_count > 0 {
            self.write_stdout(&build_terminal_frame(self.last_line_count, ""));
        }
    }

    fn write_stdout(&self, output: &str) {
        if self.output_to_stderr {
            let mut stderr = io::stderr().lock();
            let _ = stderr.write_all(output.as_bytes());
            let _ = stderr.flush();
        } else {
            let mut stdout = io::stdout().lock();
            let _ = stdout.write_all(output.as_bytes());
            let _ = stdout.flush();
        }
    }
}

fn build_terminal_frame(previous_line_count: usize, output: &str) -> String {
    if previous_line_count == 0 {
        return output.to_string();
    }

    let lines: Vec<&str> = output.lines().collect();
    let mut frame = format!("\x1b[{}A", previous_line_count);
    for line in &lines {
        frame.push_str("\x1b[2K\r");
        frame.push_str(line);
        frame.push_str("\r\n");
    }
    for _ in lines.len()..previous_line_count {
        frame.push_str("\x1b[2K\r\n");
    }
    frame
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
        assert_eq!(extract_filename("http://example.com/file.iso"), "file.iso");
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

    #[test]
    fn terminal_frame_clears_only_previous_lines() {
        let frame = build_terminal_frame(3, "new line 1\nnew line 2\n");

        assert!(frame.starts_with("\x1b[3A"));
        assert_eq!(frame.matches("\x1b[2K").count(), 3);
        assert!(!frame.contains("\x1b[J"));
        assert!(frame.contains("new line 1"));
        assert!(frame.contains("new line 2"));
    }
}
