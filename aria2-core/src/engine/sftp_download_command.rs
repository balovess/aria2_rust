use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info};

use crate::engine::command::{Command, CommandStatus};
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::filesystem::disk_writer::{DefaultDiskWriter, DiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};

pub struct SftpDownloadCommand {
    group: Arc<tokio::sync::RwLock<RequestGroup>>,
    output_path: std::path::PathBuf,
    started: bool,
    completed_bytes: u64,
    host: String,
    port: u16,
    username: String,
    password: Option<String>,
    remote_path: String,
}

impl SftpDownloadCommand {
    pub fn new(
        gid: GroupId,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
    ) -> Result<Self> {
        let (host, port, username, password, remote_path) = Self::parse_uri(uri)?;

        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| ".".to_string());

        let filename = output_name
            .map(|n| n.to_string())
            .or_else(|| Self::extract_filename(&remote_path))
            .unwrap_or_else(|| "download".to_string());

        let path = std::path::PathBuf::from(&dir).join(&filename);

        let group = RequestGroup::new(gid, vec![uri.to_string()], options.clone());
        info!(
            "SftpDownloadCommand 创建: {} -> {} ({}@{}:{}/{})",
            uri,
            path.display(),
            username,
            host,
            port,
            remote_path
        );

        Ok(Self {
            group: Arc::new(tokio::sync::RwLock::new(group)),
            output_path: path,
            started: false,
            completed_bytes: 0,
            host,
            port,
            username,
            password,
            remote_path,
        })
    }

    fn parse_uri(uri: &str) -> Result<(String, u16, String, Option<String>, String)> {
        if !uri.starts_with("sftp://") {
            return Err(Aria2Error::Fatal(FatalError::UnsupportedProtocol {
                protocol: "sftp".into(),
            }));
        }

        let without_scheme = uri.trim_start_matches("sftp://");

        let (auth_host_port, path) = match without_scheme.find('/') {
            Some(idx) => (&without_scheme[..idx], &without_scheme[idx..]),
            None => (without_scheme, "/"),
        };

        let (username, rest) = match auth_host_port.find('@') {
            Some(idx) => (&auth_host_port[..idx], &auth_host_port[idx + 1..]),
            None => {
                let user = std::env::var("USER").unwrap_or_else(|_| "root".to_string());
                return Ok((user.to_string(), 22, user, None, "/".to_string()));
            }
        };

        let (host, port) = match rest.rfind(':') {
            Some(idx) => (
                rest[..idx].to_string(),
                rest[idx + 1..].parse::<u16>().unwrap_or(22),
            ),
            None => (rest.to_string(), 22),
        };

        let password = username.split(':').nth(1).map(|p| p.to_string());
        let clean_user = username.split(':').next().unwrap_or(username).to_string();

        Ok((host, port, clean_user, password, sftp_path_decode(path)))
    }

    fn extract_filename(remote_path: &str) -> Option<String> {
        remote_path
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty() && *s != "/")
            .map(|s| s.to_string())
    }

    pub async fn group(&self) -> tokio::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.read().await
    }
}

fn sftp_path_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                result.push(byte as char);
            } else {
                result.push(c);
                result.push_str(&hex);
            }
        } else {
            result.push(c);
        }
    }
    result
}

#[async_trait]
impl Command for SftpDownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        if !self.started {
            self.group.write().await.start().await?;
            self.started = true;
        }

        debug!(
            "SFTP下载开始: {}@{}:{} -> {}",
            self.username,
            self.host,
            self.port,
            self.output_path.display()
        );

        if let Some(parent) = self.output_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    Aria2Error::Fatal(FatalError::Config(format!("创建目录失败: {}", e)))
                })?;
            }
        }

        let host = self.host.clone();
        let port = self.port;
        let username = self.username.clone();
        let password = self.password.clone();
        let remote_path = self.remote_path.clone();
        let host_err = host.clone();
        let remote_path_err = remote_path.clone();

        let download_result: Vec<u8> = tokio::task::spawn_blocking(move || {
            use std::io::Read;
            use std::net::TcpStream;

            let addr_str = format!("{}:{}", host, port);
            let addr: std::net::SocketAddr = addr_str
                .parse()
                .map_err(|_| format!("无法解析地址: {}", addr_str))?;

            let tcp = TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(15))
                .map_err(|e| format!("TCP连接失败: {}", e))?;

            tcp.set_read_timeout(Some(std::time::Duration::from_secs(30)))
                .map_err(|e| format!("设置读取超时失败: {}", e))?;

            let mut sess =
                ssh2::Session::new().map_err(|e| format!("SSH Session创建失败: {}", e))?;
            sess.set_tcp_stream(tcp);

            if let Some(ref pwd) = password {
                sess.userauth_password(&username, pwd)
                    .map_err(|e| format!("密码认证失败: {}", e))?;
            } else {
                return Err("未提供认证信息(需要password)".to_string());
            }

            let sftp = sess
                .sftp()
                .map_err(|e| format!("SFTP子系统初始化失败: {}", e))?;

            let file_size = sftp
                .stat(std::path::Path::new(&remote_path))
                .ok()
                .and_then(|s| s.size)
                .unwrap_or(0);

            let mut remote_file = sftp
                .open(std::path::Path::new(&remote_path))
                .map_err(|e| format!("打开远程文件失败 [{}]: {}", remote_path, e))?;

            let mut data = Vec::with_capacity(file_size.max(1024) as usize);
            let mut buf = [0u8; 65536];

            loop {
                match remote_file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => data.extend_from_slice(&buf[..n]),
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(format!("读取远程文件失败: {}", e)),
                }
            }

            drop(remote_file);
            Ok(data)
        })
        .await
        .map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("SFTP任务执行失败: {}", e),
            })
        })?
        .map_err(|e| {
            if e.contains("No such file") || e.contains("not found") {
                Aria2Error::Fatal(FatalError::FileNotFound {
                    path: remote_path_err.clone(),
                })
            } else if e.contains("auth") || e.contains("permission") || e.contains("denied") {
                Aria2Error::Fatal(FatalError::PermissionDenied {
                    path: format!("{}:{}", host_err, port),
                })
            } else if e.contains("连接失败") || e.contains("timeout") || e.contains("connection")
            {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            } else {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("SFTP下载错误: {}", e),
                })
            }
        })?;

        let total_length = download_result.len() as u64;
        {
            let mut g = self.group.write().await;
            g.set_total_length(total_length).await;
        }

        let raw_writer = DefaultDiskWriter::new(&self.output_path);
        let rate_limit = {
            let g = self.group.read().await;
            g.options().max_download_limit
        };
        let mut writer: Box<dyn DiskWriter> = match rate_limit {
            Some(rate) if rate > 0 => Box::new(ThrottledWriter::new(
                raw_writer,
                RateLimiter::new(&RateLimiterConfig::new(Some(rate), None)),
            )),
            _ => Box::new(raw_writer),
        };
        let start_time = Instant::now();
        let mut last_speed_update = Instant::now();
        let mut last_completed = 0u64;
        const CHUNK_SIZE: usize = 65536;

        for chunk in download_result.chunks(CHUNK_SIZE) {
            writer.write(chunk).await?;
            self.completed_bytes += chunk.len() as u64;

            {
                let g = self.group.write().await;
                g.update_progress(self.completed_bytes).await;

                let elapsed = last_speed_update.elapsed();
                if elapsed.as_millis() >= 500 {
                    let delta = self.completed_bytes - last_completed;
                    let speed = (delta as f64 / elapsed.as_secs_f64()) as u64;
                    g.update_speed(speed, 0).await;
                    last_speed_update = Instant::now();
                    last_completed = self.completed_bytes;
                }
            }
        }

        writer.finalize().await.ok();

        let final_speed = {
            let elapsed = start_time.elapsed().as_secs_f64();
            if elapsed > 0.0 {
                (self.completed_bytes as f64 / elapsed) as u64
            } else {
                0
            }
        };
        {
            let mut g = self.group.write().await;
            g.update_progress(self.completed_bytes).await;
            g.update_speed(final_speed, 0).await;
            g.complete().await?;
        }

        info!(
            "SFTP下载完成: {} ({} bytes)",
            self.output_path.display(),
            self.completed_bytes
        );
        Ok(())
    }

    fn status(&self) -> CommandStatus {
        if self.completed_bytes > 0 {
            CommandStatus::Running
        } else {
            CommandStatus::Pending
        }
    }

    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(300))
    }
}
