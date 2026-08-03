use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing::Level;
use tracing_subscriber::{
    EnvFilter,
    fmt::{self, format::FmtSpan},
    layer::{Layer, SubscriberExt},
    util::SubscriberInitExt,
};

static LOG_GUARD: OnceLock<Vec<tracing_appender::non_blocking::WorkerGuard>> = OnceLock::new();

fn parse_log_level(level_str: &str) -> Level {
    match level_str.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" | "notice" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    }
}

// ---------------------------------------------------------------------------
// Size-based rotating file writer
// ---------------------------------------------------------------------------

/// A file writer that rotates the log file when its size exceeds `max_size`
/// bytes.
///
/// `tracing_appender` only supports time-based (daily/hourly/etc.) rotation.
/// aria2 exposes `--log-max-size` / `--log-max-files` for size-based rotation,
/// so we provide our own writer and feed it to `tracing_appender::non_blocking`
/// just like a `RollingFileAppender`.
///
/// Rotated files are named `<path>.1`, `<path>.2`, …, up to `max_files` (the
/// most recent rotation is always `.1`). On rotation:
///
/// 1. The active file handle is closed (required on Windows before rename).
/// 2. The oldest rotated file `<path>.<max_files>` is deleted.
/// 3. Every `<path>.<n>` is renamed to `<path>.<n+1>` for `n = max_files-1 … 1`.
/// 4. The active file is renamed to `<path>.1`.
/// 5. A fresh empty active file is opened and `current_size` reset to 0.
///
/// This mirrors `logrotate`'s size-based scheme and keeps at most `max_files`
/// rotated files alongside the single active file.
pub struct SizeRotatingWriter {
    /// `None` only transiently while rotating — the old handle is dropped
    /// before the file is renamed (Windows requires the handle closed).
    file: Option<File>,
    path: PathBuf,
    current_size: u64,
    max_size: u64,
    max_files: usize,
}

impl SizeRotatingWriter {
    /// Open (or create) the log file at `path` in append mode, recording its
    /// current size from the filesystem so rotation resumes correctly across
    /// restarts. `max_files` is clamped to a minimum of 1.
    pub fn new(path: &Path, max_size: u64, max_files: usize) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let current_size = file.metadata()?.len();
        Ok(Self {
            file: Some(file),
            path: path.to_path_buf(),
            current_size,
            max_size,
            max_files: max_files.max(1),
        })
    }

    /// Build the rotated-file path `<path>.<n>`.
    fn rotated_path(&self, n: usize) -> PathBuf {
        let mut s = self.path.as_os_str().to_os_string();
        s.push(format!(".{}", n));
        PathBuf::from(s)
    }

    /// Close the active file, shift rotated suffixes up by one (dropping the
    /// oldest), move the active file to `.1`, and open a fresh active file.
    fn rotate(&mut self) -> io::Result<()> {
        // Close the current handle so the file can be renamed on Windows.
        if let Some(mut f) = self.file.take() {
            let _ = f.flush();
            // `f` is dropped here, releasing the OS file handle.
        }

        // Drop the oldest rotated file that would fall outside the keep window.
        let _ = std::fs::remove_file(self.rotated_path(self.max_files));

        // Shift .(n) -> .(n+1) for n = max_files-1 .. 1. Missing files are
        // ignored (rename returns an error we discard).
        for n in (1..self.max_files).rev() {
            let from = self.rotated_path(n);
            let to = self.rotated_path(n + 1);
            let _ = std::fs::rename(&from, &to);
        }

        // The active file becomes the most-recent rotation (.1).
        let _ = std::fs::rename(&self.path, self.rotated_path(1));

        // Open a fresh, empty active file.
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        self.file = Some(file);
        self.current_size = 0;
        Ok(())
    }
}

impl Write for SizeRotatingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Rotate *before* writing if this record would overflow the limit and
        // the current file is non-empty. The `current_size > 0` guard ensures a
        // single record larger than `max_size` still lands in a fresh file
        // rather than forcing an empty rotation on every call.
        if self.current_size > 0 && self.current_size + buf.len() as u64 > self.max_size {
            self.rotate()?;
        }
        let file = self.file.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "SizeRotatingWriter has no open file")
        })?;
        let n = file.write(buf)?;
        self.current_size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// Logging initialization
// ---------------------------------------------------------------------------

/// Build a `tracing_appender` daily-rotation file appender for `path`, keeping
/// at most `log_backup_count` rotated files. Used as the backward-compatible
/// fallback when size-based rotation is not requested (or fails to initialise).
fn build_daily_appender(
    p: &Path,
    log_backup_count: usize,
) -> tracing_appender::rolling::RollingFileAppender {
    let dir = p
        .parent()
        .map(|d| d.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());
    let stem = p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "aria2.log".to_string());

    use tracing_appender::rolling::Rotation;
    tracing_appender::rolling::Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix(&stem)
        .max_log_files(log_backup_count)
        .build(&dir)
        .unwrap_or_else(|_| tracing_appender::rolling::daily(&dir, &stem))
}

/// Initialize the logging subsystem with optional file output.
///
/// Sets up `tracing-subscriber` with console output and optionally writes to a
/// log file. When `log_max_size` is `Some(bytes)`, file output uses
/// [`SizeRotatingWriter`] (size-based rotation, keeping `log_max_files` rotated
/// copies, falling back to `log_backup_count` when `log_max_files` is `None`).
/// When `log_max_size` is `None`, the legacy daily time-based rotation is used
/// (controlled by `log_backup_count`).
#[allow(clippy::too_many_arguments)]
pub fn init_logging(
    log_level: &str,
    console_log_level: &str,
    log_file: Option<&str>,
    log_backup_count: usize,
    log_max_size: Option<u64>,
    log_max_files: Option<usize>,
) {
    let file_level = parse_log_level(log_level);
    let console_level = parse_log_level(console_log_level);

    let enable_file_logging = log_file.is_some_and(|f| f != "-");

    if enable_file_logging {
        let path = log_file.unwrap();
        let p = std::path::Path::new(path);

        use tracing_appender::non_blocking;

        // Size-based rotation when a max size is configured; otherwise fall
        // back to daily time-based rotation for backward compatibility.
        let (non_blocking, guard) = match log_max_size {
            Some(max_size) => match SizeRotatingWriter::new(
                p,
                max_size,
                log_max_files.unwrap_or(log_backup_count),
            ) {
                Ok(writer) => non_blocking(writer),
                Err(e) => {
                    eprintln!(
                        "[aria2] size-rotating log writer init failed ({e}); using daily rotation"
                    );
                    non_blocking(build_daily_appender(p, log_backup_count))
                }
            },
            None => non_blocking(build_daily_appender(p, log_backup_count)),
        };
        let _ = LOG_GUARD.set(vec![guard]);

        let file_filter = EnvFilter::from_default_env()
            .add_directive(file_level.into())
            .add_directive("hyper=warn".parse().unwrap())
            .add_directive("reqwest=warn".parse().unwrap());

        let console_filter = EnvFilter::from_default_env()
            .add_directive(console_level.into())
            .add_directive("hyper=warn".parse().unwrap())
            .add_directive("reqwest=warn".parse().unwrap());

        let console_layer = fmt::layer()
            .with_span_events(FmtSpan::CLOSE)
            .with_target(false)
            .with_filter(console_filter);

        let file_layer = fmt::Layer::new()
            .with_span_events(FmtSpan::CLOSE)
            .with_target(false)
            .with_writer(non_blocking)
            .with_filter(file_filter);

        let _ = tracing_subscriber::registry()
            .with(console_layer)
            .with(file_layer)
            .try_init();
    } else {
        let env_filter = EnvFilter::from_default_env()
            .add_directive(console_level.into())
            .add_directive("hyper=warn".parse().unwrap())
            .add_directive("reqwest=warn".parse().unwrap());

        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(
                fmt::layer()
                    .with_span_events(FmtSpan::CLOSE)
                    .with_target(false),
            )
            .try_init();
    }

    tracing::info!("Log system initialization complete");
}

#[cfg(test)]
mod tests {
    use super::{SizeRotatingWriter, parse_log_level};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;
    use tracing::Level;

    /// Read a file as a string, returning an empty string if it does not exist.
    fn read_file(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    /// Build the rotated-file path `<path>.<n>` (mirrors `SizeRotatingWriter`).
    fn rotated(path: &Path, n: usize) -> PathBuf {
        let mut s = path.as_os_str().to_os_string();
        s.push(format!(".{}", n));
        PathBuf::from(s)
    }

    #[test]
    fn parse_log_level_recognizes_all_levels() {
        assert_eq!(parse_log_level("trace"), Level::TRACE);
        assert_eq!(parse_log_level("TRACE"), Level::TRACE);
        assert_eq!(parse_log_level("debug"), Level::DEBUG);
        assert_eq!(parse_log_level("info"), Level::INFO);
        assert_eq!(parse_log_level("notice"), Level::INFO);
        assert_eq!(parse_log_level("warn"), Level::WARN);
        assert_eq!(parse_log_level("error"), Level::ERROR);
        // Unknown levels fall back to INFO.
        assert_eq!(parse_log_level("bogus"), Level::INFO);
    }

    #[test]
    fn no_rotation_when_under_threshold() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("app.log");
        {
            let mut w = SizeRotatingWriter::new(&path, 100, 3).unwrap();
            w.write_all(b"small record").unwrap();
            w.flush().unwrap();
        }
        assert_eq!(read_file(&path), "small record");
        // Nothing rotated yet.
        assert!(!rotated(&path, 1).exists());
    }

    #[test]
    fn rotate_when_exceeding_max_size() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("app.log");
        {
            let mut w = SizeRotatingWriter::new(&path, 10, 3).unwrap();
            // 5 bytes: 0 + 5 = 5, not > 10, no rotation.
            w.write_all(b"first").unwrap();
            w.flush().unwrap();
            // 5 + 6 = 11 > 10 with non-empty file -> rotate, then write.
            w.write_all(b"second").unwrap();
            w.flush().unwrap();
        }
        // Active file holds the record written after rotation.
        assert_eq!(read_file(&path), "second");
        // The pre-rotation content moved to .1.
        assert_eq!(read_file(&rotated(&path, 1)), "first");
    }

    #[test]
    fn keeps_at_most_max_files_rotated_files() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("app.log");
        {
            let mut w = SizeRotatingWriter::new(&path, 5, 3).unwrap();
            // Seed the active file with 4 bytes (4 <= 5, no rotation).
            w.write_all(b"seed").unwrap();
            w.flush().unwrap();
            // Each 6-byte record forces a rotation (current non-empty + overflow).
            for i in 0..5u8 {
                let rec = [b'A' + i; 6];
                w.write_all(&rec).unwrap();
                w.flush().unwrap();
            }
        }
        // max_files = 3: only .1, .2, .3 may exist.
        assert!(rotated(&path, 1).exists());
        assert!(rotated(&path, 2).exists());
        assert!(rotated(&path, 3).exists());
        assert!(!rotated(&path, 4).exists());
        // The active file holds the most recent record.
        assert_eq!(
            read_file(&path),
            String::from_utf8(vec![b'A' + 4; 6]).unwrap()
        );
    }

    #[test]
    fn append_to_existing_file_resumes_size() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("app.log");
        // Pre-create a file with 8 bytes.
        std::fs::write(&path, b"12345678").unwrap();
        {
            let mut w = SizeRotatingWriter::new(&path, 10, 3).unwrap();
            // current_size is read from metadata (= 8). A 5-byte write would
            // reach 13 > 10, so rotation fires immediately, moving the existing
            // 8 bytes to .1 before writing the new record.
            w.write_all(b"abcde").unwrap();
            w.flush().unwrap();
        }
        assert_eq!(read_file(&path), "abcde");
        assert_eq!(read_file(&rotated(&path, 1)), "12345678");
    }
}
