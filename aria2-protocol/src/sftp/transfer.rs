use tracing::{debug, info, warn};

use tokio::io::AsyncSeekExt;

use super::file_ops::{FileAttributes, OpenFlags, SftpFileOps};
use super::session::SftpSession;

const TRANSFER_BUF_SIZE: usize = 64 * 1024;
const PROGRESS_REPORT_INTERVAL: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy)]
pub enum TransferMode {
    Binary,
    Text,
}

impl Default for TransferMode {
    fn default() -> Self { Self::Binary }
}

#[derive(Debug, Clone)]
pub struct TransferProgress {
    pub bytes_transferred: u64,
    pub total_bytes: u64,
    pub speed_bytes_per_sec: f64,
    pub elapsed_secs: f64,
}

impl TransferProgress {
    pub fn percent(&self) -> f64 {
        if self.total_bytes == 0 {
            100.0
        } else {
            (self.bytes_transferred as f64 / self.total_bytes as f64) * 100.0
        }
    }

    pub fn is_complete(&self) -> bool {
        self.bytes_transferred >= self.total_bytes || self.total_bytes == 0
    }
}

#[derive(Debug, Clone)]
pub struct TransferOptions {
    pub buffer_size: usize,
    pub resume_offset: u64,
    pub mode: TransferMode,
    pub preserve_permissions: bool,
    pub preserve_time: bool,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            buffer_size: TRANSFER_BUF_SIZE,
            resume_offset: 0,
            mode: TransferMode::Binary,
            preserve_permissions: false,
            preserve_time: false,
        }
    }
}

impl TransferOptions {
    pub fn with_resume(mut self, offset: u64) -> Self {
        self.resume_offset = offset;
        self
    }

    pub fn with_buffer_size(mut self, size: usize) -> Self {
        self.buffer_size = size.max(1024).min(1024 * 1024);
        self
    }

    pub fn preserve_metadata(mut self) -> Self {
        self.preserve_permissions = true;
        self.preserve_time = true;
        self
    }
}

pub struct SftpTransfer<'a> {
    ops: SftpFileOps<'a>,
}

impl<'a> SftpTransfer<'a> {
    pub fn new(session: &'a SftpSession) -> Self {
        Self { ops: SftpFileOps::new(session) }
    }

    pub fn ops(&self) -> &SftpFileOps<'a> {
        &self.ops
    }

    pub async fn download(
        &self,
        remote_path: &str,
        local_path: &std::path::Path,
        options: &TransferOptions,
    ) -> Result<TransferProgress, String> {
        info!("SFTP download start: {} -> {}", remote_path, local_path.display());

        let remote_attr = self.ops.lstat(remote_path)?;
        if !remote_attr.is_regular_file {
            return Err(format!("remote path is not a regular file: {}", remote_path));
        }

        let total_size = remote_attr.size;
        let start_offset = if options.resume_offset > 0 {
            options.resume_offset.min(total_size)
        } else {
            0
        };

        debug!("file size: {}, start offset: {}", total_size, start_offset);

        let mut remote_file = self.ops.open(remote_path, OpenFlags::READ, 0)?;

        let mut local_file = if start_offset > 0 && local_path.exists() {
            tokio::fs::OpenOptions::new()
                .write(true)
                .open(local_path)
                .await
                .map_err(|e| format!("open local file failed: {}", e))?
        } else {
            tokio::fs::File::create(local_path)
                .await
                .map_err(|e| format!("create local file failed: {}", e))?
        };

        if start_offset > 0 {
            local_file.set_len(start_offset).await
                .map_err(|e| format!("set file length failed: {}", e))?;
            local_file.seek(std::io::SeekFrom::Start(start_offset)).await
                .map_err(|e| format!("seek to resume position failed: {}", e))?;
        }

        let mut buf = vec![0u8; options.buffer_size];
        let mut transferred = start_offset;
        let start_time = std::time::Instant::now();
        let mut last_report = start_offset;

        loop {
            let remaining = total_size.saturating_sub(transferred);
            if remaining == 0 {
                break;
            }

            let to_read = (options.buffer_size as u64).min(remaining) as usize;
            let n = remote_file.read_at(transferred, &mut buf[..to_read])?;

            if n == 0 {
                break;
            }

            tokio::io::AsyncWriteExt::write_all(&mut local_file, &buf[..n]).await
                .map_err(|e| format!("write local file failed: {}", e))?;

            transferred += n as u64;

            if transferred.saturating_sub(last_report) >= PROGRESS_REPORT_INTERVAL {
                last_report = transferred;
                let elapsed = start_time.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 {
                    (transferred - start_offset) as f64 / elapsed
                } else {
                    0.0
                };
                debug!("progress: {:.1}% ({}/{}, {:.1} KB/s)",
                    (transferred as f64 / total_size as f64) * 100.0,
                    transferred, total_size,
                    speed / 1024.0);
            }
        }

        remote_file.close()?;
        drop(local_file);

        let elapsed = start_time.elapsed().as_secs_f64();
        let avg_speed = if elapsed > 0.0 {
            (transferred - start_offset) as f64 / elapsed
        } else {
            0.0
        };

        let progress = TransferProgress {
            bytes_transferred: transferred,
            total_bytes: total_size,
            speed_bytes_per_sec: avg_speed,
            elapsed_secs: elapsed,
        };

        info!("SFTP download done: {:.1}%, {:.1} KB/s, {:.1}s",
            progress.percent(), avg_speed / 1024.0, elapsed);

        Ok(progress)
    }

    pub async fn upload(
        &self,
        local_path: &std::path::Path,
        remote_path: &str,
        options: &TransferOptions,
    ) -> Result<TransferProgress, String> {
        info!("SFTP upload start: {} -> {}", local_path.display(), remote_path);

        let metadata = tokio::fs::metadata(local_path).await
            .map_err(|e| format!("get local file metadata failed: {}", e))?;
        let total_size = metadata.len();

        let mut remote_file = self.ops.open(remote_path, OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE, 0o644)?;

        let start_offset = if options.resume_offset > 0 {
            options.resume_offset.min(total_size)
        } else if options.resume_offset == 0 {
            let rstat = self.ops.lstat(remote_path);
            match rstat {
                Ok(attr) if attr.is_regular_file => attr.size.min(total_size),
                _ => 0,
            }
        } else {
            0
        };

        if start_offset > 0 {
            debug!("resume mode, start offset: {}", start_offset);
        }

        let mut local_file = tokio::fs::File::open(local_path).await
            .map_err(|e| format!("open local file failed: {}", e))?;

        if start_offset > 0 {
            local_file.seek(std::io::SeekFrom::Start(start_offset)).await
                .map_err(|e| format!("seek to resume position failed: {}", e))?;
        }

        let mut buf = vec![0u8; options.buffer_size];
        let mut transferred = start_offset;
        let start_time = std::time::Instant::now();
        let mut last_report = start_offset;

        loop {
            let remaining = total_size.saturating_sub(transferred);
            if remaining == 0 {
                break;
            }

            let to_read = (options.buffer_size as u64).min(remaining) as usize;
            let n = tokio::io::AsyncReadExt::read(&mut local_file, &mut buf[..to_read]).await
                .map_err(|e| format!("read local file failed: {}", e))?;

            if n == 0 {
                break;
            }

            remote_file.write_at(transferred, &buf[..n])?;
            transferred += n as u64;

            if transferred.saturating_sub(last_report) >= PROGRESS_REPORT_INTERVAL {
                last_report = transferred;
                let elapsed = start_time.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 {
                    (transferred - start_offset) as f64 / elapsed
                } else {
                    0.0
                };
                debug!("upload progress: {:.1}% ({}/{}, {:.1} KB/s)",
                    (transferred as f64 / total_size as f64) * 100.0,
                    transferred, total_size,
                    speed / 1024.0);
            }
        }

        remote_file.fsync()?;
        remote_file.close()?;
        drop(local_file);

        let elapsed = start_time.elapsed().as_secs_f64();
        let avg_speed = if elapsed > 0.0 {
            (transferred - start_offset) as f64 / elapsed
        } else {
            0.0
        };

        if options.preserve_permissions {
            #[cfg(unix)]
            let perm = metadata.permissions().mode() as u32;
            #[cfg(not(unix))]
            let perm = 0o644u32;

            if let Err(e) = self.ops.set_stat(remote_path, &FileAttributes {
                permissions: perm,
                ..Default::default()
            }) {
                warn!("set remote file permissions failed: {}", e);
            }
        }

        let progress = TransferProgress {
            bytes_transferred: transferred,
            total_bytes: total_size,
            speed_bytes_per_sec: avg_speed,
            elapsed_secs: elapsed,
        };

        info!("SFTP upload done: {:.1}%, {:.1} KB/s, {:.1}s",
            progress.percent(), avg_speed / 1024.0, elapsed);

        Ok(progress)
    }

    pub fn get_remote_size(&self, remote_path: &str) -> Result<u64, String> {
        let attr = self.ops.stat(remote_path)?;
        Ok(attr.size)
    }

    pub fn check_resume_support(&self, remote_path: &str) -> Result<Option<u64>, String> {
        match self.ops.stat(remote_path) {
            Ok(attr) if attr.is_regular_file => Ok(Some(attr.size)),
            Ok(_) => Ok(None),
            Err(_) => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transfer_options_defaults() {
        let opts = TransferOptions::default();
        assert_eq!(opts.buffer_size, TRANSFER_BUF_SIZE);
        assert_eq!(opts.resume_offset, 0);
        assert!(matches!(opts.mode, TransferMode::Binary));
        assert!(!opts.preserve_permissions);
        assert!(!opts.preserve_time);
    }

    #[test]
    fn test_transfer_options_builder() {
        let opts = TransferOptions::default()
            .with_resume(4096)
            .with_buffer_size(128 * 1024)
            .preserve_metadata();
        assert_eq!(opts.resume_offset, 4096);
        assert_eq!(opts.buffer_size, 131072);
        assert!(opts.preserve_permissions);
        assert!(opts.preserve_time);
    }

    #[test]
    fn test_transfer_options_buffer_clamp() {
        let small = TransferOptions::default().with_buffer_size(512);
        assert_eq!(small.buffer_size, 1024);

        let large = TransferOptions::default().with_buffer_size(2048 * 1024);
        assert_eq!(large.buffer_size, 1024 * 1024);
    }

    #[test]
    fn test_progress_percent_zero_total() {
        let prog = TransferProgress {
            bytes_transferred: 0,
            total_bytes: 0,
            speed_bytes_per_sec: 0.0,
            elapsed_secs: 0.0,
        };
        assert!((prog.percent() - 100.0).abs() < 0.001);
        assert!(prog.is_complete());
    }

    #[test]
    fn test_progress_partial() {
        let prog = TransferProgress {
            bytes_transferred: 500,
            total_bytes: 1000,
            speed_bytes_per_sec: 250.0,
            elapsed_secs: 2.0,
        };
        assert!((prog.percent() - 50.0).abs() < 0.01);
        assert!(!prog.is_complete());
    }

    #[test]
    fn test_progress_complete() {
        let prog = TransferProgress {
            bytes_transferred: 1000,
            total_bytes: 1000,
            speed_bytes_per_sec: 500.0,
            elapsed_secs: 2.0,
        };
        assert!((prog.percent() - 100.0).abs() < 0.01);
        assert!(prog.is_complete());
    }

    #[test]
    fn test_transfer_mode_variants() {
        assert!(matches!(TransferMode::default(), TransferMode::Binary));
        let modes = [TransferMode::Binary, TransferMode::Text];
        for m in &modes {
            let _ = format!("{:?}", m);
        }
    }

    #[test]
    fn test_constants() {
        assert_eq!(TRANSFER_BUF_SIZE, 65536);
        assert_eq!(PROGRESS_REPORT_INTERVAL, 262144);
    }
}
