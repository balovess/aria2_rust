use tokio::io::{AsyncReadExt, AsyncWriteExt, AsyncSeekExt, SeekFrom};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};

use super::connection::FtpConnection;

const DEFAULT_BUFFER_SIZE: usize = 65536;

#[derive(Debug, Clone)]
pub struct FtpDownloadOptions {
    pub buffer_size: usize,
    pub resume_offset: Option<u64>,
    pub max_retries: u32,
}

impl Default for FtpDownloadOptions {
    fn default() -> Self {
        Self {
            buffer_size: DEFAULT_BUFFER_SIZE,
            resume_offset: None,
            max_retries: 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub speed_bytes_per_sec: f64,
}

pub struct FtpDownload<'a> {
    conn: &'a mut FtpConnection,
    options: FtpDownloadOptions,
}

impl<'a> FtpDownload<'a> {
    pub fn new(conn: &'a mut FtpConnection, options: Option<FtpDownloadOptions>) -> Self {
        Self {
            conn,
            options: options.unwrap_or_default(),
        }
    }

    pub async fn download_file(
        &mut self,
        remote_path: &str,
        local_path: &str,
        progress_callback: Option<fn(DownloadProgress)>,
    ) -> Result<DownloadResult, String> {
        self.conn.type_image().await?;

        let file_size = self.conn.size(remote_path).await.ok();

        if let Some(offset) = self.options.resume_offset {
            self.conn.rest(offset).await?;
        }

        let (data_host, data_port) = if self.conn.options.passive_mode {
            self.conn.pasv().await?
        } else {
            return Err("主动模式暂未支持".to_string())
        };

        self.conn.retr(remote_path).await?;

        let mut data_stream: TcpStream = timeout(
            Duration::from_secs(30),
            TcpStream::connect((data_host.as_str(), data_port)),
        ).await
        .map_err(|_| "FTP数据连接超时")?
        .map_err(|e| format!("FTP数据连接失败: {}", e))?;

        let mut file = tokio::fs::File::create(local_path).await
            .map_err(|e| format!("创建本地文件失败: {}", e))?;

        if let Some(offset) = self.options.resume_offset {
            file.seek(SeekFrom::Start(offset)).await
                .map_err(|e| format!("设置文件偏移失败: {}", e))?;
        }

        let mut buffer = vec![0u8; self.options.buffer_size];
        let mut total_downloaded = self.options.resume_offset.unwrap_or(0);
        let start_time = std::time::Instant::now();

        loop {
            let bytes_read = data_stream.read(&mut buffer).await
                .map_err(|e| format!("读取FTP数据流失败: {}", e))?;

            if bytes_read == 0 {
                break;
            }

            file.write_all(&buffer[..bytes_read]).await
                .map_err(|e| format!("写入本地文件失败: {}", e))?;

            total_downloaded += bytes_read as u64;

            if let Some(cb) = progress_callback {
                let elapsed = start_time.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 { total_downloaded as f64 / elapsed } else { 0.0 };
                cb(DownloadProgress {
                    downloaded_bytes: total_downloaded,
                    total_bytes: file_size,
                    speed_bytes_per_sec: speed,
                });
            }
        }

        file.flush().await.map_err(|e| format!("刷新文件缓冲区失败: {}", e))?;
        drop(data_stream);

        let final_resp = self.conn.read_response().await?;
        if !final_resp.is_positive_completion() {
            return Err(format!("下载完成但服务器报告错误: {} {}", final_resp.code, final_resp.message));
        }

        let elapsed = start_time.elapsed().as_secs_f64();
        let avg_speed = if elapsed > 0.0 { total_downloaded as f64 / elapsed } else { 0.0 };

        Ok(DownloadResult {
            file_path: local_path.to_string(),
            bytes_downloaded: total_downloaded,
            total_size: file_size,
            success: true,
            average_speed_bps: avg_speed,
            duration_secs: elapsed,
        })
    }

    pub async fn download_to_memory(
        &mut self,
        remote_path: &str,
    ) -> Result<Vec<u8>, String> {
        self.conn.type_image().await?;

        let file_size = self.conn.size(remote_path).await.ok();

        if let Some(offset) = self.options.resume_offset {
            self.conn.rest(offset).await?;
        }

        let (data_host, data_port) = self.conn.pasv().await?;
        self.conn.retr(remote_path).await?;

        let mut data_stream = timeout(Duration::from_secs(30), TcpStream::connect((data_host.as_str(), data_port)))
            .await
            .map_err(|_| "FTP数据连接超时")?
            .map_err(|e| format!("FTP数据连接失败: {}", e))?;

        let mut result = Vec::with_capacity(file_size.unwrap_or(1024 * 1024) as usize);
        let mut buffer = vec![0u8; self.options.buffer_size];

        loop {
            let bytes_read = data_stream.read(&mut buffer).await
                .map_err(|e| format!("读取FTP数据流失败: {}", e))?;

            if bytes_read == 0 {
                break;
            }

            result.extend_from_slice(&buffer[..bytes_read]);
        }

        drop(data_stream);
        let _final_resp = self.conn.read_response().await?;

        Ok(result)
    }
}

#[derive(Debug, Clone)]
pub struct DownloadResult {
    pub file_path: String,
    pub bytes_downloaded: u64,
    pub total_size: Option<u64>,
    pub success: bool,
    pub average_speed_bps: f64,
    pub duration_secs: f64,
}

impl DownloadResult {
    pub fn is_complete(&self) -> bool {
        self.success && match self.total_size {
            Some(total) => self.bytes_downloaded >= total,
            None => self.bytes_downloaded > 0,
        }
    }

    pub fn human_readable_size(bytes: u64) -> String {
        const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
        let mut size = bytes as f64;
        let mut unit_idx = 0;
        while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
            size /= 1024.0;
            unit_idx += 1;
        }
        if unit_idx == 0 {
            format!("{} {}", bytes, UNITS[unit_idx])
        } else {
            format!("{:.2} {}", size, UNITS[unit_idx])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_human_readable_size() {
        assert_eq!(DownloadResult::human_readable_size(500), "500 B");
        assert_eq!(DownloadResult::human_readable_size(1024), "1.00 KB");
        assert_eq!(DownloadResult::human_readable_size(1536), "1.50 KB");
        assert_eq!(DownloadResult::human_readable_size(1048576), "1.00 MB");
        assert_eq!(DownloadResult::human_readable_size(1073741824), "1.00 GB");
    }

    #[test]
    fn test_download_result_complete() {
        let full = DownloadResult {
            file_path: "test.bin".into(),
            bytes_downloaded: 1000,
            total_size: Some(1000),
            success: true,
            average_speed_bps: 1000.0,
            duration_secs: 1.0,
        };
        assert!(full.is_complete());

        let partial = DownloadResult {
            file_path: "test.bin".into(),
            bytes_downloaded: 500,
            total_size: Some(1000),
            success: true,
            average_speed_bps: 500.0,
            duration_secs: 1.0,
        };
        assert!(!partial.is_complete());

        let unknown_total = DownloadResult {
            file_path: "test.bin".into(),
            bytes_downloaded: 100,
            total_size: None,
            success: true,
            average_speed_bps: 100.0,
            duration_secs: 1.0,
        };
        assert!(unknown_total.is_complete());
    }

    #[test]
    fn test_ftp_download_options_default() {
        let opts = FtpDownloadOptions::default();
        assert_eq!(opts.buffer_size, DEFAULT_BUFFER_SIZE);
        assert!(opts.resume_offset.is_none());
        assert_eq!(opts.max_retries, 3);
    }
}
