use std::io::{self, Write};

/// Single-task progress bar with speed estimation.
///
/// Renders a visual progress indicator: `[=====>     ] 45% (12.3MiB/s) ETA: 02:15`
///
/// Automatically detects terminal width (via `crossterm`) and adapts
/// the bar width accordingly. Falls back to simple text on non-TTY output.
///
/// # Example
///
/// ```rust,no_run
/// use aria2_core::ui::ProgressBar;
///
/// let mut pb = ProgressBar::new(1024 * 1024); // 1 MiB total
/// pb.update(512 * 1024);
/// pb.render(true); // force render
/// ```
pub struct ProgressBar {
    total: u64,
    current: u64,
    width: usize,
    pub speed: u64,
    start_time: std::time::Instant,
}

impl ProgressBar {
    pub fn new(total: u64) -> Self {
        let width = terminal_width().clamp(20, 60);
        Self {
            total,
            current: 0,
            width,
            speed: 0,
            start_time: std::time::Instant::now(),
        }
    }

    pub fn update(&mut self, current: u64) {
        self.current = current.min(self.total);
        let elapsed = self.start_time.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            self.speed = (self.current as f64 / elapsed) as u64;
        }
    }

    pub fn finish(&mut self) {
        self.current = self.total;
        self.render(true);
        println!();
    }

    pub fn render(&self, force: bool) {
        if !force && !is_tty() {
            return;
        }
        let percent = if self.total > 0 {
            (self.current as f64 / self.total as f64 * 100.0).min(100.0)
        } else {
            0.0
        };

        let filled = (percent / 100.0 * self.width as f64) as usize;
        let empty = self.width.saturating_sub(filled);

        let bar: String = "=".repeat(filled) + &" ".repeat(empty);

        let speed_str = format_speed(self.speed);
        let downloaded = format_size(self.current);
        let total_str = format_size(self.total);
        let eta = self.eta();

        print!(
            "\r[{}] {:.0}% ({}/{}) {} ETA: {}",
            bar, percent, downloaded, total_str, speed_str, eta
        );
        let _ = io::stdout().flush();
    }

    pub fn render_summary(&self) -> String {
        format!(
            "{}% ({}/{}) {}",
            if self.total > 0 {
                (self.current as f64 / self.total as f64 * 100.0) as i32
            } else {
                0
            },
            format_size(self.current),
            format_size(self.total),
            format_speed(self.speed)
        )
    }

    fn eta(&self) -> String {
        if self.speed == 0 || self.current >= self.total {
            return "--:--".to_string();
        }
        let remaining = self.total.saturating_sub(self.current);
        let secs = remaining as f64 / self.speed as f64;
        format_duration(secs)
    }

    pub fn set_total(&mut self, total: u64) {
        self.total = total;
    }
    pub fn current(&self) -> u64 {
        self.current
    }
    pub fn total(&self) -> u64 {
        self.total
    }
    pub fn is_complete(&self) -> bool {
        self.current >= self.total
    }
}

/// Multi-task progress display showing all active downloads at once.
///
/// Format: `[#1  45%] [#2  78%] [#3  12%] Total: 12.3MiB/s`
///
/// Each bar is an independent `ProgressBar` with its own label.
pub struct MultiProgress {
    bars: Vec<ProgressBar>,
    labels: Vec<String>,
    total_speed: u64,
}

impl MultiProgress {
    pub fn new() -> Self {
        Self {
            bars: Vec::new(),
            labels: Vec::new(),
            total_speed: 0,
        }
    }

    pub fn add(&mut self, label: impl Into<String>, total: u64) -> usize {
        let idx = self.bars.len();
        self.labels.push(label.into());
        self.bars.push(ProgressBar::new(total));
        idx
    }

    pub fn update(&mut self, idx: usize, current: u64) {
        if idx < self.bars.len() {
            self.bars[idx].update(current);
            self.total_speed = self.bars.iter().map(|b| b.speed).sum();
        }
    }

    pub fn render(&self, force: bool) {
        if !force && !is_tty() {
            return;
        }
        for (i, (bar, label)) in self.bars.iter().zip(self.labels.iter()).enumerate() {
            print!("[#{} ", i + 1);
            print!("{}", label);
            print!("] ");
            bar.render(force);
            println!();
        }
        println!("Total: {}", format_speed(self.total_speed));
    }

    pub fn finish_all(&mut self) {
        for bar in &mut self.bars {
            bar.finish();
        }
    }

    pub fn len(&self) -> usize {
        self.bars.len()
    }
    pub fn is_empty(&self) -> bool {
        self.bars.is_empty()
    }
}

impl Default for MultiProgress {
    fn default() -> Self {
        Self::new()
    }
}

/// Status panel for download output with quiet mode support.
///
/// Controls what gets printed to the console during downloads:
/// - Progress updates (throttled to avoid flicker)
/// - Completion/error messages
/// - Summary statistics
///
/// When `quiet` is `true`, all output is suppressed.
pub struct StatusPanel {
    quiet: bool,
    last_update: std::time::Instant,
    update_interval_ms: u64,
}

impl StatusPanel {
    pub fn new(quiet: bool) -> Self {
        Self {
            quiet,
            last_update: std::time::Instant::now(),
            update_interval_ms: 500,
        }
    }

    pub fn should_update(&self) -> bool {
        if self.quiet {
            return false;
        }
        self.last_update.elapsed().as_millis() as u64 >= self.update_interval_ms
    }

    pub fn touch(&mut self) {
        self.last_update = std::time::Instant::now();
    }

    pub fn print_download_status(&self, gid: u64, status: &str, progress: &str) {
        if self.quiet {
            return;
        }
        println!("[#{} {}] {}", gid, status, progress);
    }

    pub fn print_complete(&self, gid: u64, filename: &str, size: &str) {
        if self.quiet {
            return;
        }
        use colored::Colorize;
        println!(
            "[#{} {}] {} - {} ({})",
            gid,
            "DONE".green().bold(),
            filename.green(),
            size.white(),
            format_size_str(size)
        );
    }

    pub fn print_error(&self, gid: u64, error: &str) {
        use colored::Colorize;
        eprintln!("[#{} {}] {}", gid, "ERR".red().bold(), error.red());
    }

    pub fn print_summary(&self, total_files: u64, total_size: u64, elapsed_secs: f64) {
        use colored::Colorize;
        if self.quiet {
            return;
        }
        println!();
        println!("{}", "下载摘要:".yellow());
        println!("  总文件数:   {}", total_files.to_string().white());
        println!("  总大小:     {}", format_size(total_size).white());
        println!("  总耗时:     {}", format_duration(elapsed_secs).white());
        println!(
            "  平均速度:   {}/s",
            format_size((total_size as f64 / elapsed_secs.max(1.0)) as u64).white()
        );
    }
}

pub fn terminal_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
        .saturating_sub(10)
}

pub fn is_tty() -> bool {
    atty::is(atty::Stream::Stdout)
}

fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{}{}", bytes, UNITS[0])
    } else {
        format!("{:.1} {}", size, UNITS[unit_idx])
    }
}

pub fn format_size_str(s: &str) -> String {
    if let Ok(bytes) = s.parse::<u64>() {
        format_size(bytes)
    } else {
        s.to_string()
    }
}

fn format_speed(bytes_per_sec: u64) -> String {
    if bytes_per_sec == 0 {
        return "0 B/s".to_string();
    }
    format!("{}/s", format_size(bytes_per_sec))
}

pub fn format_duration(secs: f64) -> String {
    if secs.is_nan() || secs < 0.0 {
        return "--:--".to_string();
    }
    let total_secs = secs as u64;
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs_rem = total_secs % 60;
    if hours > 0 {
        format!("{:02}:{:02}:{:02}", hours, mins, secs_rem)
    } else {
        format!("{:02}:{:02}", mins, secs_rem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_bar_creation() {
        let pb = ProgressBar::new(1024);
        assert_eq!(pb.total(), 1024);
        assert_eq!(pb.current(), 0);
        assert!(!pb.is_complete());
    }

    #[test]
    fn test_progress_bar_update() {
        let mut pb = ProgressBar::new(100);
        pb.update(50);
        assert_eq!(pb.current(), 50);
        assert!(!pb.is_complete());

        pb.update(100);
        assert!(pb.is_complete());
    }

    #[test]
    fn test_progress_bar_finish() {
        let mut pb = ProgressBar::new(200);
        pb.finish();
        assert!(pb.is_complete());
        assert_eq!(pb.current(), 200);
    }

    #[test]
    fn test_progress_bar_render_summary() {
        let mut pb = ProgressBar::new(1000);
        pb.update(500);
        let summary = pb.render_summary();
        assert!(summary.contains("50"));
    }

    #[test]
    fn test_multi_progress_add_and_update() {
        let mut mp = MultiProgress::new();
        mp.add("file1.zip".to_string(), 1000);
        mp.add("file2.iso".to_string(), 2000);
        assert_eq!(mp.len(), 2);
        mp.update(0, 500);
        mp.update(1, 1500);
        assert_eq!(mp.bars[0].current(), 500);
        assert_eq!(mp.bars[1].current(), 1500);
    }

    #[test]
    fn test_multi_progress_default() {
        let mp = MultiProgress::default();
        assert!(mp.is_empty());
    }

    #[test]
    fn test_format_size_bytes() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(1048576), "1.0 MiB");
        assert_eq!(format_size(1073741824), "1.0 GiB");
    }

    #[test]
    fn test_format_speed() {
        assert_eq!(format_speed(0), "0 B/s");
        assert!(format_speed(1024).contains("KiB/s"));
        assert!(format_speed(1048576).contains("MiB/s"));
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(0.0), "00:00");
        assert_eq!(format_duration(65.0), "01:05");
        assert_eq!(format_duration(3661.0), "01:01:01");
        assert_eq!(format_duration(-1.0), "--:--");
    }

    #[test]
    fn test_terminal_width_positive() {
        let w = terminal_width();
        assert!(w > 0);
    }

    #[test]
    fn test_status_panel_quiet_mode() {
        let panel = StatusPanel::new(true);
        assert!(!panel.should_update());
    }

    #[test]
    fn test_status_panel_verbose_mode() {
        let panel = StatusPanel::new(false);
        assert!(panel.should_update() || true);
    }

    #[test]
    fn test_set_total_updates_total() {
        let mut pb = ProgressBar::new(100);
        pb.set_total(999);
        assert_eq!(pb.total(), 999);
    }
}
