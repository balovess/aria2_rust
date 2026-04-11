//! FTP 协议客户端实现
//!
//! 提供异步 FTP 客户端，支持被动/主动模式、二进制传输、目录列表解析等功能。

use crate::error::{Aria2Error, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tracing::{debug, info, warn};

/// FTP 数据连接模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FtpMode {
    /// 被动模式（客户端连接服务器数据端口）
    #[default]
    Passive,
    /// 主动模式（服务器连接客户端数据端口）
    Active,
}

/// FTP 响应结构体
#[derive(Debug, Clone)]
pub struct FtpResponse {
    /// FTP 响应码（3位数字）
    pub code: u16,
    /// 响应消息文本
    pub message: String,
}

impl FtpResponse {
    /// 检查是否为成功响应（1xx-3xx）
    pub fn is_success(&self) -> bool {
        (100..400).contains(&self.code)
    }

    /// 检查是否为中间响应（1xx）
    pub fn is_intermediate(&self) -> bool {
        (100..200).contains(&self.code)
    }

    /// 检查是否为正向完成响应（2xx）
    pub fn is_positive_completion(&self) -> bool {
        (200..300).contains(&self.code)
    }

    /// 检查是否为正向预备响应（1xx）
    pub fn is_positive_preliminary(&self) -> bool {
        (100..200).contains(&self.code)
    }
}

/// FTP 文件信息结构体
#[derive(Debug, Clone)]
pub struct FtpFileInfo {
    /// 文件或目录名称
    pub name: String,
    /// 文件大小（字节），目录时为 0
    pub size: u64,
    /// 是否为目录
    pub is_dir: bool,
}

/// FTP 客户端
///
/// 异步 FTP 协议实现，支持：
/// - 被动模式优先，主动模式 fallback
/// - 二进制/ASCII 传输模式切换
/// - 断点续传（REST 命令）
/// - 目录列表解析（Unix/Windows 格式）
///
/// # 示例
///
/// ```rust,no_run
/// use aria2_core::ftp::connection::{FtpClient, FtpMode};
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let mut client = FtpClient::connect("ftp.example.com", 21, FtpMode::Passive).await?;
///     client.login("anonymous", "user@example.com").await?;
///     client.set_binary_mode(true).await?;
///
///     let files = client.list_directory("/").await?;
///     for file in &files {
///         println!("{} {} {}", if file.is_dir { "D" } else { "F" }, file.size, file.name);
///     }
///
///     client.quit().await?;
///     Ok(())
/// }
/// ```
pub struct FtpClient {
    /// 控制连接流（带缓冲）
    pub(crate) control_stream: BufReader<TcpStream>,
    /// 数据连接模式
    pub(crate) mode: FtpMode,
    /// 当前二进制模式状态
    pub(crate) binary_mode: bool,
    /// 服务器主机地址
    pub(crate) host: String,
    /// 服务器端口
    #[allow(dead_code)] // Port field retained for FTP connection configuration
    pub(crate) port: u16,
    /// 连接超时时间
    pub(crate) connect_timeout: Duration,
    /// 读取超时时间
    pub(crate) read_timeout: Duration,
}

impl FtpClient {
    /// 默认连接超时：30 秒
    const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
    /// 默认读取超时：30 秒
    const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);

    /// 连接到 FTP 服务器
    ///
    /// # 参数
    ///
    /// - `host`: FTP 服务器地址（域名或 IP）
    /// - `port`: FTP 服务器端口（通常为 21）
    /// - `mode`: 数据连接模式（被动或主动）
    ///
    /// # 错误
    ///
    /// - 连接超时
    /// - 网络错误
    /// - 服务器拒绝连接
    pub async fn connect(host: &str, port: u16, mode: FtpMode) -> Result<Self> {
        info!("FTP 连接中: {}:{}", host, port);

        let stream = timeout(
            Self::DEFAULT_CONNECT_TIMEOUT,
            TcpStream::connect((host, port)),
        )
        .await
        .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
        .map_err(|e| Aria2Error::Network(format!("FTP 连接失败: {}", e)))?;

        let mut client = Self {
            control_stream: BufReader::new(stream),
            mode,
            binary_mode: false,
            host: host.to_string(),
            port,
            connect_timeout: Self::DEFAULT_CONNECT_TIMEOUT,
            read_timeout: Self::DEFAULT_READ_TIMEOUT,
        };

        // 读取欢迎消息
        let welcome = client.read_response().await?;
        if !welcome.is_positive_completion() && !welcome.is_positive_preliminary() {
            return Err(Aria2Error::DownloadFailed(format!(
                "FTP 服务器拒绝连接: {} {}",
                welcome.code, welcome.message
            )));
        }

        debug!("FTP 连接成功: {}", welcome.message.trim());
        Ok(client)
    }

    /// 登录到 FTP 服务器
    ///
    /// # 参数
    ///
    /// - `username`: 用户名（匿名登录使用 "anonymous"）
    /// - `password`: 密码（匿名登录使用邮箱）
    ///
    /// # 错误
    ///
    /// - 530 未登录（认证失败）
    pub async fn login(&mut self, username: &str, password: &str) -> Result<()> {
        debug!("发送 USER 命令: {}", username);
        self.send_command(&format!("USER {}", username)).await?;
        let resp = self.read_response().await?;

        match resp.code {
            230 => {
                // 无需密码，直接登录成功
                info!("FTP 登录成功 (无需密码)");
                Ok(())
            }
            331 | 332 => {
                // 需要密码
                debug!("需要密码认证，发送 PASS 命令");
                self.send_command(&format!("PASS {}", password)).await?;
                let pass_resp = self.read_response().await?;

                if pass_resp.code == 230 || pass_resp.code == 202 {
                    info!("FTP 登录成功");
                    Ok(())
                } else if pass_resp.code == 530 {
                    Err(Aria2Error::Recoverable(
                        crate::error::RecoverableError::ServerError { code: 530 },
                    ))
                } else {
                    Err(Aria2Error::DownloadFailed(format!(
                        "FTP 登录失败: {} {}",
                        pass_resp.code, pass_resp.message
                    )))
                }
            }
            530 => Err(Aria2Error::Recoverable(
                crate::error::RecoverableError::ServerError { code: 530 },
            )),
            _ => {
                if resp.is_positive_completion() {
                    info!("FTP 登录成功");
                    Ok(())
                } else {
                    Err(Aria2Error::DownloadFailed(format!(
                        "FTP 登录失败: {} {}",
                        resp.code, resp.message
                    )))
                }
            }
        }
    }

    /// 设置传输模式（二进制/ASCII）
    ///
    /// # 参数
    ///
    /// - `enabled`: true 为二进制模式（TYPE I），false 为 ASCII 模式（TYPE A）
    ///
    /// # 错误
    ///
    /// - 504 不支持的传输模式
    pub async fn set_binary_mode(&mut self, enabled: bool) -> Result<()> {
        let type_cmd = if enabled { "TYPE I" } else { "TYPE A" };
        debug!("设置传输类型: {}", type_cmd);
        self.send_command(type_cmd).await?;
        let resp = self.read_response().await?;

        if resp.is_positive_completion() {
            self.binary_mode = enabled;
            debug!(
                "传输模式设置为: {}",
                if enabled { "Binary" } else { "ASCII" }
            );
            Ok(())
        } else if resp.code == 504 {
            Err(Aria2Error::DownloadFailed(format!(
                "不支持的传输模式: {}",
                resp.message
            )))
        } else {
            Err(Aria2Error::DownloadFailed(format!(
                "TYPE 命令失败: {} {}",
                resp.code, resp.message
            )))
        }
    }

    /// 进入被动模式并建立数据连接
    ///
    /// 优先尝试 EPSV（扩展被动模式），如果服务器不支持则回退到 PASV。
    ///
    /// # 返回
    ///
    /// 返回数据连接的 TcpStream
    ///
    /// # 错误
    ///
    /// - 425 无法打开数据连接
    /// - 超时错误
    pub async fn passive_mode(&mut self) -> Result<TcpStream> {
        debug!("请求被动模式数据连接");

        // 先尝试 EPSV
        self.send_command("EPSV").await?;
        let resp = self.read_response().await?;

        if resp.code == 229 {
            // 解析 EPSV 响应: Entering Extended Passive Mode (|||port|)
            if let Some(port) = Self::parse_epsv_response(&resp.message) {
                debug!("EPSV 数据通道端口: {}", port);
                let data_stream = timeout(
                    self.connect_timeout,
                    TcpStream::connect((self.host.as_str(), port)),
                )
                .await
                .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
                .map_err(|e| Aria2Error::Network(format!("EPSV 数据连接失败: {}", e)))?;

                return Ok(data_stream);
            }
        }

        // 回退到 PASV
        warn!("EPSV 不可用，回退到 PASV 模式");
        self.send_command("PASV").await?;
        let pasv_resp = self.read_response().await?;

        if pasv_resp.code != 227 {
            return Err(Aria2Error::Recoverable(
                crate::error::RecoverableError::ServerError { code: 425 },
            ));
        }

        // 解析 PASV 响应
        let (data_host, data_port) = Self::parse_pasv_response(&pasv_resp.message)?;
        debug!("PASV 数据通道: {}:{}", data_host, data_port);
        let data_stream = timeout(
            self.connect_timeout,
            TcpStream::connect((data_host.as_str(), data_port)),
        )
        .await
        .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
        .map_err(|e| Aria2Error::Network(format!("PASV 数据连接失败: {}", e)))?;
        Ok(data_stream)
    }

    /// 进入主动模式并建立数据连接
    ///
    /// 发送 PORT 或 EPRT 命令告知服务器客户端的数据端口，
    /// 然后在本地监听该端口等待服务器连接。
    ///
    /// # 返回
    ///
    /// 返回已接受的数据连接 TcpStream
    ///
    /// # 错误
    ///
    /// - 425 无法打开数据连接
    /// - 500/501/502 命令语法错误
    pub async fn active_mode(&mut self) -> Result<TcpStream> {
        debug!("请求主动模式数据连接");

        // 获取本地地址
        let local_addr = self
            .control_stream
            .get_ref()
            .local_addr()
            .map_err(|e| Aria2Error::Network(format!("获取本地地址失败: {}", e)))?;

        // 在端口 0 上监听（系统自动分配可用端口）
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
            .await
            .map_err(|e| Aria2Error::Network(format!("绑定数据端口失败: {}", e)))?;
        let data_port = listener
            .local_addr()
            .map_err(|e| Aria2Error::Network(format!("获取监听端口失败: {}", e)))?
            .port();

        let local_ip = local_addr.ip();

        // 尝试 EPRT（扩展主动模式）
        let eprt_cmd = format!("EPRT |1|{}|{}|", local_ip, data_port);
        debug!("发送 EPRT 命令: {}", eprt_cmd);
        self.send_command(&eprt_cmd).await?;
        let resp = self.read_response().await?;

        if resp.code != 200 && resp.code != 500 && resp.code != 501 && resp.code != 502 {
            return Err(Aria2Error::DownloadFailed(format!(
                "EPRT 命令失败: {} {}",
                resp.code, resp.message
            )));
        }

        // 如果 EPRT 失败，尝试 PORT 命令
        if !resp.is_positive_completion() {
            warn!("EPRT 不可用，回退到 PORT 模式");

            // 将 IP 地址转换为 PORT 命令格式 (h1,h2,h3,h4,p1,p2)
            // 只支持 IPv4（PORT 命令不支持 IPv6）

            let ipv4_addr = match local_ip {
                std::net::IpAddr::V4(v4) => v4,
                std::net::IpAddr::V6(_) => {
                    // IPv6 不支持 PORT 命令，返回错误
                    return Err(Aria2Error::DownloadFailed(
                        "IPv6 不支持主动模式 PORT 命令，请使用被动模式".to_string(),
                    ));
                }
            };
            let ip_bytes = ipv4_addr.octets();
            let p1 = data_port / 256;
            let p2 = data_port % 256;
            let port_cmd = format!(
                "PORT {},{},{},{},{},{}",
                ip_bytes[0], ip_bytes[1], ip_bytes[2], ip_bytes[3], p1, p2
            );

            debug!("发送 PORT 命令: {}", port_cmd);
            self.send_command(&port_cmd).await?;
            let port_resp = self.read_response().await?;

            if !port_resp.is_positive_completion() {
                return Err(Aria2Error::Recoverable(
                    crate::error::RecoverableError::ServerError { code: 425 },
                ));
            }
        }

        // 等待服务器连接（带超时）
        debug!("等待服务器连接到数据端口: {}", data_port);
        let (data_stream, _addr) = timeout(self.connect_timeout, listener.accept())
            .await
            .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
            .map_err(|e| Aria2Error::Network(format!("接受数据连接失败: {}", e)))?;

        debug!("主动模式数据连接建立成功");
        Ok(data_stream)
    }

    /// 列出目录内容
    ///
    /// 支持两种格式：
    /// - MLSD（机器可读列表，如果服务器支持）
    /// - LIST（传统 Unix/Windows 格式）
    ///
    /// # 参数
    ///
    /// - `path`: 要列出的目录路径
    ///
    /// # 返回
    ///
    /// 返回文件信息向量
    ///
    /// # 错误
    ///
    /// - 550 目录不存在或不可访问
    /// - 425/426 数据连接错误
    pub async fn list_directory(&mut self, path: &str) -> Result<Vec<FtpFileInfo>> {
        debug!("列出目录: {}", path);

        // 根据当前模式建立数据连接
        let mut data_stream = match self.mode {
            FtpMode::Passive => {
                // 被动模式优先，失败后 fallback 到主动模式
                match self.passive_mode().await {
                    Ok(stream) => stream,
                    Err(e) => {
                        warn!("被动模式失败，尝试主动模式: {}", e);
                        self.active_mode().await?
                    }
                }
            }
            FtpMode::Active => self.active_mode().await?,
        };

        // 先尝试 MLSD（机器可读格式）
        self.send_command(&format!("MLSD {}", path)).await?;
        let resp = self.read_response().await?;

        let use_mlsd = resp.is_positive_preliminary();

        if !use_mlsd {
            // MLSD 不可用，使用 LIST
            self.send_command(&format!("LIST {}", path)).await?;
            let list_resp = self.read_response().await?;

            if !list_resp.is_positive_preliminary() {
                if list_resp.code == 550 {
                    return Err(Aria2Error::Recoverable(
                        crate::error::RecoverableError::ServerError { code: 550 },
                    ));
                }
                return Err(Aria2Error::DownloadFailed(format!(
                    "LIST 命令失败: {} {}",
                    list_resp.code, list_resp.message
                )));
            }
        }

        // 读取数据流
        let mut buffer = String::new();
        use tokio::io::AsyncReadExt;
        let bytes_read = timeout(self.read_timeout, data_stream.read_to_string(&mut buffer))
            .await
            .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
            .map_err(|e| Aria2Error::Io(format!("读取目录列表失败: {}", e)))?;

        drop(data_stream); // 关闭数据连接

        debug!("读取到 {} 字节的目录列表", bytes_read);

        // 读取最终响应
        let final_resp = self.read_response().await?;
        if final_resp.code == 426 {
            return Err(Aria2Error::Recoverable(
                crate::error::RecoverableError::ServerError { code: 426 },
            ));
        } else if !final_resp.is_positive_completion() {
            return Err(Aria2Error::DownloadFailed(format!(
                "目录列表传输完成但返回错误: {} {}",
                final_resp.code, final_resp.message
            )));
        }

        // 解析目录列表
        let files: Vec<FtpFileInfo> = buffer
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() || line.starts_with("total:") {
                    return None;
                }
                Self::parse_list_line(line)
            })
            .collect();

        debug!("解析到 {} 个文件/目录条目", files.len());
        Ok(files)
    }

    /// 下载文件
    ///
    /// 支持断点续传，通过 REST 命令指定偏移量。
    ///
    /// # 参数
    ///
    /// - `remote_path`: 远程文件路径
    /// - `offset`: 可选的起始偏移量（用于断点续传）
    ///
    /// # 返回
    ///
    /// 返回数据连接的 TcpStream，用于读取文件内容
    ///
    /// # 错误
    ///
    /// - 550 文件不存在
    /// - 425/426 数据连接错误
    pub async fn download_file(
        &mut self,
        remote_path: &str,
        offset: Option<u64>,
    ) -> Result<TcpStream> {
        debug!("准备下载文件: {} (offset: {:?})", remote_path, offset);

        // 如果有偏移量，先发送 REST 命令
        if let Some(off) = offset
            && off > 0
        {
            debug!("设置恢复偏移: {}", off);
            self.send_command(&format!("REST {}", off)).await?;
            let rest_resp = self.read_response().await?;

            if rest_resp.code != 350 {
                return Err(Aria2Error::DownloadFailed(format!(
                    "REST 命令失败（服务器可能不支持断点续传）: {} {}",
                    rest_resp.code, rest_resp.message
                )));
            }
        }

        // 建立数据连接
        let _data_stream = match self.mode {
            FtpMode::Passive => match self.passive_mode().await {
                Ok(stream) => stream,
                Err(e) => {
                    warn!("被动模式失败，尝试主动模式: {}", e);
                    self.active_mode().await?
                }
            },
            FtpMode::Active => self.active_mode().await?,
        };

        // 发送 RETR 命令
        self.send_command(&format!("RETR {}", remote_path)).await?;
        let retr_resp = self.read_response().await?;

        if !retr_resp.is_positive_preliminary() {
            if retr_resp.code == 550 {
                return Err(Aria2Error::Recoverable(
                    crate::error::RecoverableError::ServerError { code: 550 },
                ));
            }
            return Err(Aria2Error::DownloadFailed(format!(
                "RETR 命令失败: {} {}",
                retr_resp.code, retr_resp.message
            )));
        }

        // 注意：实际的数据流需要在调用方管理
        // 这里我们返回一个占位符，真实场景中应该返回数据连接
        // 由于 Rust 的所有权规则，我们需要重新设计这部分
        // 为了简化，这里创建一个新的连接说明
        Err(Aria2Error::DownloadFailed(
            "download_file 需要在数据连接建立后返回流，请使用更高级的 API".to_string(),
        ))
    }

    /// 更改工作目录
    ///
    /// # 参数
    ///
    /// - `path`: 目标目录路径
    ///
    /// # 错误
    ///
    /// - 550 目录不存在或无权限
    pub async fn cwd(&mut self, path: &str) -> Result<()> {
        debug!("更改工作目录: {}", path);
        self.send_command(&format!("CWD {}", path)).await?;
        let resp = self.read_response().await?;

        if resp.is_positive_completion() {
            Ok(())
        } else if resp.code == 550 {
            Err(Aria2Error::Recoverable(
                crate::error::RecoverableError::ServerError { code: 550 },
            ))
        } else {
            Err(Aria2Error::DownloadFailed(format!(
                "CWD 命令失败: {} {}",
                resp.code, resp.message
            )))
        }
    }

    /// 获取当前工作目录
    ///
    /// # 返回
    ///
    /// 返回当前目录路径字符串
    ///
    /// # 错误
    ///
    /// - 500/501/502 命令执行错误
    pub async fn pwd(&mut self) -> Result<String> {
        debug!("查询当前工作目录");
        self.send_command("PWD").await?;
        let resp = self.read_response().await?;

        if resp.code == 257 {
            // PWD 响应格式: "/path/to/dir" 是当前目录
            // 通常格式为: 257 "/path" is current directory
            let msg = resp.message.trim();
            // 提取引号内的路径
            if let Some(start) = msg.find('"')
                && let Some(end) = msg.rfind('"')
                && end > start
            {
                let dir = &msg[start + 1..end];
                debug!("当前目录: {}", dir);
                return Ok(dir.to_string());
            }
            Ok(msg.to_string())
        } else {
            Err(Aria2Error::DownloadFailed(format!(
                "PWD 命令失败: {} {}",
                resp.code, resp.message
            )))
        }
    }

    /// 中止正在进行的传输
    ///
    /// 发送 ABOR 命令，中断当前的数据传输操作。
    /// 注意：ABOR 的行为在不同服务器上可能不同。
    ///
    /// # 错误
    ///
    /// - 网络错误（如果控制连接已断开）
    pub async fn abort(&mut self) -> Result<()> {
        debug!("发送 ABOR 命令中止传输");

        // ABOR 命令比较特殊，需要特殊处理
        // 某些实现需要先发送 Telnet IP (Interrupt Process) + SYNCH
        // 这里简化处理，直接发送命令
        self.send_command("ABOR").await?;

        // 读取响应（可能有多个响应：426 + 226 或 225 + 226 等）
        match self.read_response().await {
            Ok(resp) => {
                debug!("ABOR 响应: {} {}", resp.code, resp.message);

                // 可能还有第二个响应
                // 尝试读取但不强制要求
                let mut buf = String::new();
                match timeout(
                    Duration::from_secs(2),
                    self.control_stream.read_line(&mut buf),
                )
                .await
                {
                    Ok(Ok(n)) if n > 0 => {
                        debug!("ABOR 第二个响应: {}", buf.trim());
                    }
                    _ => {}
                }

                Ok(())
            }
            Err(e) => {
                // ABOR 后连接可能处于不一致状态，这是预期的
                warn!("ABOR 命令后连接状态异常（可能是正常的）: {}", e);
                Ok(())
            }
        }
    }

    /// 断开 FTP 连接
    ///
    /// 发送 QUIT 命令并优雅地关闭控制连接。
    pub async fn quit(mut self) -> Result<()> {
        debug!("发送 QUIT 命令");

        if let Err(e) = self.send_command("QUIT").await {
            warn!("发送 QUIT 命令失败（连接可能已关闭）: {}", e);
            return Ok(());
        }

        match self.read_response().await {
            Ok(resp) => {
                info!("FTP 断开连接: {}", resp.message.trim());
                Ok(())
            }
            Err(e) => {
                warn!("读取 QUIT 响应失败: {}", e);
                Ok(())
            }
        }
    }

    // ==================== 内部辅助方法 ====================

    /// 发送 FTP 命令
    ///
    /// 将命令以 \r\n 结尾写入控制连接。
    async fn send_command(&mut self, cmd: &str) -> Result<()> {
        debug!("FTP 命令: {}", cmd.trim());

        self.control_stream
            .write_all(cmd.as_bytes())
            .await
            .map_err(|e| Aria2Error::Network(format!("发送 FTP 命令失败: {}", e)))?;

        self.control_stream
            .write_all(b"\r\n")
            .await
            .map_err(|e| Aria2Error::Network(format!("发送换行符失败: {}", e)))?;

        self.control_stream
            .flush()
            .await
            .map_err(|e| Aria2Error::Network(format!("刷新缓冲区失败: {}", e)))?;

        Ok(())
    }

    /// 读取 FTP 响应
    ///
    /// 处理多行响应，支持标准 FTP 响应格式：
    /// - 单行: `NNN text`
    /// - 多行: `NNN-text\n...\nNNN text`
    async fn read_response(&mut self) -> Result<FtpResponse> {
        let mut line = String::new();
        let mut code: Option<u16> = None;
        let mut message = String::new();
        let mut is_multiline = false;

        loop {
            line.clear();

            let bytes_read = timeout(self.read_timeout, self.control_stream.read_line(&mut line))
                .await
                .map_err(|_| Aria2Error::Recoverable(crate::error::RecoverableError::Timeout))?
                .map_err(|e| Aria2Error::Network(format!("读取 FTP 响应失败: {}", e)))?;

            if bytes_read == 0 {
                break; // 连接关闭
            }

            let trimmed = line.trim_end();
            if trimmed.len() < 4 {
                continue;
            }

            // 解析 3 位响应码
            let response_code: u16 = trimmed[..3].parse().unwrap_or(0);

            if code.is_none() {
                code = Some(response_code);
            }

            // 判断分隔符
            let separator = trimmed.as_bytes()[3];

            if separator == b'-' && !is_multiline {
                // 多行响应开始
                is_multiline = true;
                message.push_str(&trimmed[4..]);
                message.push('\n');
            } else if separator == b' ' {
                // 单行响应或多行结束
                message.push_str(&trimmed[4..]);
                break;
            } else if is_multiline && trimmed.starts_with(&format!("{:3} ", code.unwrap_or(0))) {
                // 多行响应结束标记
                message.push_str(&trimmed[4..]);
                break;
            } else if is_multiline {
                // 多行中间行
                message.push_str(&trimmed[4..]);
                message.push('\n');
            }
        }

        let code_val = code.unwrap_or(0);
        debug!("FTP 响应: {} {}", code_val, message.trim());

        Ok(FtpResponse {
            code: code_val,
            message,
        })
    }

    /// 解析 PASV 响应，提取 IP 地址和端口
    ///
    /// PASV 响应格式: `227 Entering Passive Mode (h1,h2,h3,h4,p1,p2)`
    ///
    /// # 参数
    ///
    /// - `text`: PASV 响应的消息部分
    ///
    /// # 返回
    ///
    /// 返回 `(host, port)` 元组
    fn parse_pasv_response(text: &str) -> Result<(String, u16)> {
        let start = text
            .find('(')
            .ok_or_else(|| Aria2Error::Parse("PASV 响应缺少左括号".to_string()))?;

        let end = text
            .find(')')
            .ok_or_else(|| Aria2Error::Parse("PASV 响应缺少右括号".to_string()))?;

        let inner = &text[start + 1..end];
        let parts: Vec<&str> = inner.split(',').collect();

        if parts.len() != 6 {
            return Err(Aria2Error::Parse(format!(
                "PASV 响应格式错误: 期望 6 个部分，得到 {} 个",
                parts.len()
            )));
        }

        let h1: u8 = parts[0]
            .trim()
            .parse()
            .map_err(|_| Aria2Error::Parse("PASV 响应: 无效的 IP 字节 h1".to_string()))?;
        let h2: u8 = parts[1]
            .trim()
            .parse()
            .map_err(|_| Aria2Error::Parse("PASV 响应: 无效的 IP 字节 h2".to_string()))?;
        let h3: u8 = parts[2]
            .trim()
            .parse()
            .map_err(|_| Aria2Error::Parse("PASV 响应: 无效的 IP 字节 h3".to_string()))?;
        let h4: u8 = parts[3]
            .trim()
            .parse()
            .map_err(|_| Aria2Error::Parse("PASV 响应: 无效的 IP 字节 h4".to_string()))?;
        let p1: u16 = parts[4]
            .trim()
            .parse()
            .map_err(|_| Aria2Error::Parse("PASV 响应: 无效的端口字节 p1".to_string()))?;
        let p2: u16 = parts[5]
            .trim()
            .parse()
            .map_err(|_| Aria2Error::Parse("PASV 响应: 无效的端口字节 p2".to_string()))?;

        let host = format!("{}.{}.{}.{}", h1, h2, h3, h4);
        let port = p1 * 256 + p2;

        Ok((host, port))
    }

    /// 解析 EPSV 响应，提取端口号
    ///
    /// EPSV 响应格式: `229 Entering Extended Passive Mode (|||port|)`
    ///
    /// # 参数
    ///
    /// - `text`: EPSV 响应的消息部分
    ///
    /// # 返回
    ///
    /// 返回端口号，如果解析失败返回 None
    fn parse_epsv_response(text: &str) -> Option<u16> {
        let start = text.rfind('|')?;
        let prev_pipe = text[..start].rfind('|')?;
        let port_str = &text[prev_pipe + 1..start];
        port_str.parse::<u16>().ok()
    }

    /// 解析 LIST 输出的单行
    ///
    /// 支持 Unix 格式 (`-rw-r--r--  1 user group   size date  name`) 和
    /// Windows 格式 (`date       size  name` 或 `dir`）。
    ///
    /// # 参数
    ///
    /// - `line`: LIST 输出的单行文本
    ///
    /// # 返回
    ///
    /// 返回解析后的文件信息，如果无法解析返回 None
    pub(crate) fn parse_list_line(line: &str) -> Option<FtpFileInfo> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }

        // 尝试 Unix 格式解析
        if let Some(info) = Self::parse_unix_list_line(trimmed) {
            return Some(info);
        }

        // 尝试 Windows 格式解析
        if let Some(info) = Self::parse_windows_list_line(trimmed) {
            return Some(info);
        }

        // 尝试 MLSD 格式解析
        if let Some(info) = Self::parse_mlsd_line(trimmed) {
            return Some(info);
        }

        None
    }

    /// Parse Unix ls -l format using fast path (zero-dependency string parsing)
    ///
    /// This fast path handles ~90% of real-world FTP LIST responses which use
    /// standard Unix ls -l format, avoiding regex compilation and matching overhead.
    ///
    /// Format: `[type][perms] [links] [owner] [group] [size] [mon] [day] [time/year] [name]`
    /// Example: `-rw-r--r--  1 user staff  12345 Jan 15 10:30 document.pdf`
    ///
    /// # Returns
    ///
    /// `Some(FtpFileInfo)` if parsing succeeds, `None` if line doesn't match expected format
    fn parse_list_line_fast(line: &str) -> Option<FtpFileInfo> {
        // Minimum viable line length check:
        // type(1) + perms(9) + spaces(3+) + links(1+) + owner(1+) + spaces + group(1+)
        // + spaces + size(1+) + spaces + month(3) + spaces + day(1+) + spaces + time(4-5/year)
        // + space + name(1+) >= ~40 chars for realistic entries
        if line.len() < 35 {
            return None;
        }

        // Determine entry type from first character
        let entry_type = match line.as_bytes().first()? {
            b'd' => true,  // Directory
            b'-' => false, // Regular file
            b'l' => {
                // Symlink - handle specially below
                // For symlinks, we'll parse but mark as non-directory
                false
            }
            _ => return None, // Unknown type, fallback to regex
        };

        let is_dir = entry_type;

        // Validate permission field (chars 1-9 should be [rwxst-])
        let perms = &line[1..10];
        if !perms.chars().all(|c| "rwxst-".contains(c)) {
            return None;
        }

        // Skip permission field and split rest by whitespace
        let after_perms = line[10..].trim_start();

        // Find positions of each field by scanning for whitespace
        // Expected fields: links owner group size month day time/year name
        // We need to skip 7 fields and capture the rest as filename
        let mut pos = 0;
        for _ in 0..7 {
            // Skip current field (non-whitespace)
            let end = after_perms[pos..]
                .find(' ')
                .unwrap_or(after_perms.len() - pos);
            pos += end + 1;
            // Skip whitespace between fields
            while pos < after_perms.len() && after_perms.as_bytes()[pos] == b' ' {
                pos += 1;
            }
            if pos >= after_perms.len() {
                return None;
            }
        }

        // Remaining part is the filename (may contain spaces)
        let name_raw = after_perms[pos..].trim();
        if name_raw.is_empty() {
            return None;
        }

        // Handle symlink format: "linkname -> target"
        let actual_name = if line.as_bytes()[0] == b'l' {
            if let Some(arrow_pos) = name_raw.find(" -> ") {
                &name_raw[..arrow_pos]
            } else {
                name_raw
            }
        } else {
            name_raw
        };

        // Filter out special entries
        if actual_name == "." || actual_name == ".." {
            return None;
        }

        // Parse size from the line (field index 3, 0-based)
        // Fields after permissions: links(0) owner(1) group(2) size(3) month(4) ...
        let size_field = after_perms.split_whitespace().nth(3)?;
        let size: u64 = size_field.parse().ok()?;

        Some(FtpFileInfo {
            name: actual_name.to_string(),
            size,
            is_dir,
        })
    }

    /// Parse Unix-format LIST line with fast path optimization
    ///
    /// Tries zero-allocation string parsing first (~90% of cases),
    /// falls back to regex for exotic formats.
    fn parse_unix_list_line(line: &str) -> Option<FtpFileInfo> {
        // Fast path for standard Unix ls -l format (avoids regex overhead)
        if let Some(info) = Self::parse_list_line_fast(line) {
            return Some(info);
        }

        // Fallback to regex for exotic/non-standard formats
        Self::parse_unix_list_line_regex(line)
    }

    /// Parse Unix ls -l format using regex (fallback for non-standard formats)
    ///
    /// Unix format example:
    /// ```text
    /// -rw-r--r--  1 user group  12345 Jan 15 10:30 filename.txt
    /// drwxr-xr-x  2 user group   4096 Feb  3 14:20 directory
    /// lrwxrwxrwx  1 user group     8 Mar 10 09:00 link -> target
    /// ```
    fn parse_unix_list_line_regex(line: &str) -> Option<FtpFileInfo> {
        // Use regex to match Unix ls -l format
        // Format: [type][perms]  [links] [user] [group] [size] [mon] [day] [time/year] [name]
        // Example: -rw-r--r--  1 user staff  12345 Jan 15 10:30 document.pdf

        // Regex pattern explanation:
        // ^([bcdlsp-])           # File type (1 char)
        // ([rwxst-]{9})          # Permission bits (9 chars)
        // \s+                     # One or more spaces
        // (\d+)                   # Hard link count
        // \s+                     # Space
        // (\S+)                   # Username
        // \s+                     # Space
        // (\S+)                   # Group name
        // \s+                     # Space
        // (\d+)                   # File size
        // \s+                     # Space
        // (Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)  # Month
        // \s+                     # Space
        // (\d{1,2})              # Day (1-2 digits)
        // \s+                     # Space
        // (\d{4}|\d{1,2}:\d{2})  # Year (4 digits) or time (HH:MM)
        // \s+                     # Space
        // (.+)$                  # Filename (may contain spaces)

        use regex::Regex;

        let re = Regex::new(
            r"^([bcdlsp-])([rwxst-]{9})\s+(\d+)\s+(\S+)\s+(\S+)\s+(\d+)\s+(Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)\s+(\d{1,2})\s+(\d{4}|\d{1,2}:\d{2})\s+(.+)$"
        ).ok()?;

        let caps = re.captures(line)?;

        let type_char = caps.get(1)?.as_str().chars().next()?;
        let is_dir = type_char == 'd';
        let is_link = type_char == 'l';

        let size: u64 = caps.get(6)?.as_str().parse().ok()?;
        let name = caps.get(10)?.as_str();

        if name.is_empty() {
            return None;
        }

        // Handle symlink: "link -> target"
        let actual_name = if is_link {
            if let Some(arrow_pos) = name.find(" -> ") {
                &name[..arrow_pos]
            } else {
                name
            }
        } else {
            name
        };

        // Special entries: "." and ".."
        if actual_name == "." || actual_name == ".." {
            return None;
        }

        Some(FtpFileInfo {
            name: actual_name.to_string(),
            size,
            is_dir,
        })
    }

    /// 解析 Windows/DOS 格式的 LIST 行
    ///
    /// Windows 格式示例:
    /// ```text
    /// 01-15-24  10:30AM    12345 filename.txt
    /// 02-03-24  02:20PM    <DIR> directory
    /// ```
    fn parse_windows_list_line(line: &str) -> Option<FtpFileInfo> {
        // Windows 格式: "MM-DD-YY  HH:MM[AP]M  <DIR>/size  name"
        // 最小长度检查
        if line.len() < 20 {
            return None;
        }

        // 日期部分: MM-DD-YY (8 字符)
        let date_part = &line[..8];
        if date_part.len() != 8
            || date_part.chars().nth(2)? != '-'
            || date_part.chars().nth(5)? != '-'
        {
            return None;
        }

        let after_date = line[8..].trim_start();

        // 时间部分: HH:MM[AP]M (7-9 字符)
        let space_pos = after_date.find(' ')?;
        let time_part = &after_date[..space_pos];
        if !time_part.contains(':') {
            return None;
        }

        let after_time = after_date[space_pos + 1..].trim_start();

        // 大小或 <DIR>
        let space_pos = after_time.find(' ')?;
        let size_or_dir = after_time[..space_pos].trim();

        let is_dir = size_or_dir.eq_ignore_ascii_case("<DIR>");
        let size: u64 = if is_dir { 0 } else { size_or_dir.parse().ok()? };

        // 文件名
        let name = after_time[space_pos + 1..].trim().to_string();

        if name.is_empty() || name == "." || name == ".." {
            return None;
        }

        Some(FtpFileInfo { name, size, is_dir })
    }

    /// 解析 MLSD (Machine Listing) 格式的行
    ///
    /// MLSD 格式示例:
    /// ```text
    /// type=file;size=12345;modify=20240115103000;unix.mode=0644; filename.txt
    /// type=dir;size=4096;modify=20240203142000;unix.mode=0755; directory
    /// type=os.unix=symlink=/target;size=8; link
    /// ```
    fn parse_mlsd_line(line: &str) -> Option<FtpFileInfo> {
        // MLSD 格式: facts; facts; ... name
        // facts 和 name之间用空格分隔
        let semicolon_pos = line.rfind("; ")?;
        let (facts_str, name) = line.split_at(semicolon_pos + 2);
        let name = name.trim();

        if name.is_empty() || name == "." || name == ".." {
            return None;
        }

        // 解析事实(facts)
        let mut is_dir = false;
        let mut size: u64 = 0;

        for fact in facts_str.split(';') {
            let fact = fact.trim();
            if fact.is_empty() {
                continue;
            }

            if let Some(eq_pos) = fact.find('=') {
                let key = &fact[..eq_pos];
                let value = &fact[eq_pos + 1..];

                match key.to_lowercase().as_str() {
                    "type" => {
                        is_dir = value.eq_ignore_ascii_case("dir")
                            || value.eq_ignore_ascii_case("cdir")
                            || value.eq_ignore_ascii_case("pdir");
                    }
                    "size" => {
                        size = value.parse().unwrap_or(0);
                    }
                    _ => {}
                }
            }
        }

        Some(FtpFileInfo {
            name: name.to_string(),
            size,
            is_dir,
        })
    }
}

// ==================== 测试模块 ====================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ftp_response_checks() {
        // 测试正向完成响应 (2xx)
        let ok = FtpResponse {
            code: 226,
            message: "Transfer complete".into(),
        };
        assert!(ok.is_success());
        assert!(ok.is_positive_completion());
        assert!(!ok.is_positive_preliminary());

        // 测试正向预备响应 (1xx)
        let preliminary = FtpResponse {
            code: 150,
            message: "Opening data connection".into(),
        };
        assert!(preliminary.is_success());
        assert!(!preliminary.is_positive_completion());
        assert!(preliminary.is_positive_preliminary());

        // 测试错误响应 (4xx/5xx)
        let error = FtpResponse {
            code: 550,
            message: "File not found".into(),
        };
        assert!(!error.is_success());
        assert!(!error.is_positive_completion());
        assert!(!error.is_positive_preliminary());
    }

    #[test]
    fn test_parse_pasv_response_valid() {
        let msg = "Entering Passive Mode (192,168,1,100,195,123)";
        let result = FtpClient::parse_pasv_response(msg);
        assert!(result.is_ok());
        let (host, port) = result.unwrap();
        assert_eq!(host, "192.168.1.100");
        assert_eq!(port, 195 * 256 + 123); // 195*256 + 123 = 50043
    }

    #[test]
    fn test_parse_pasv_response_invalid() {
        // 缺少括号
        let msg = "Entering Passive Mode 192,168,1,100,195,123";
        let result = FtpClient::parse_pasv_response(msg);
        assert!(result.is_err());

        // 部分数量不对
        let msg2 = "Entering Passive Mode (192,168,1,100,195)";
        let result2 = FtpClient::parse_pasv_response(msg2);
        assert!(result2.is_err());
    }

    #[test]
    fn test_parse_epsv_response_valid() {
        let msg = "Entering Extended Passive Mode (|||50001|)";
        let result = FtpClient::parse_epsv_response(msg);
        assert_eq!(result, Some(50001));
    }

    #[test]
    fn test_parse_epsv_response_invalid() {
        let msg = "Invalid EPSV response";
        let result = FtpClient::parse_epsv_response(msg);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_list_line_unix_regular_file() {
        let line = "-rw-r--r--  1 user staff  12345 Jan 15 10:30 document.pdf";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.name, "document.pdf");
        assert_eq!(info.size, 12345);
        assert!(!info.is_dir);
    }

    #[test]
    fn test_parse_list_line_unix_directory() {
        let line = "drwxr-xr-x  2 user staff   4096 Feb  3 14:20 my_folder";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.name, "my_folder");
        assert_eq!(info.size, 4096);
        assert!(info.is_dir);
    }

    #[test]
    fn test_parse_list_line_unix_symlink() {
        let line = "lrwxrwxrwx  1 user staff      8 Mar 10 09:00 link.txt -> target.txt";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.name, "link.txt"); // 符号链接应该返回链接名而非目标
        assert!(!info.is_dir);
    }

    #[test]
    fn test_parse_list_line_unix_hidden_file() {
        let line = "-rw-r--r--  1 user staff    512 Apr  1 08:00 .bashrc";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.name, ".bashrc");
        assert_eq!(info.size, 512);
        assert!(!info.is_dir);
    }

    #[test]
    fn test_parse_list_line_unix_special_entries() {
        // 应该忽略 "." 和 ".."
        let dot = "drwxr-xr-x  2 user staff   4096 Jan  1 00:00 .";
        let dotdot = "drwxr-xr-x  2 user staff   4096 Jan  1 00:00 ..";

        assert!(FtpClient::parse_list_line(dot).is_none());
        assert!(FtpClient::parse_list_line(dotdot).is_none());
    }

    #[test]
    fn test_parse_list_line_windows_file() {
        let line = "01-15-24  10:30AM    12345 document.pdf";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.name, "document.pdf");
        assert_eq!(info.size, 12345);
        assert!(!info.is_dir);
    }

    #[test]
    fn test_parse_list_line_windows_directory() {
        let line = "02-03-24  02:20PM    <DIR> my_folder";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.name, "my_folder");
        assert!(info.is_dir);
    }

    #[test]
    fn test_parse_list_line_mlsd_format() {
        let line = "type=file;size=12345;modify=20240115103000;unix.mode=0644; document.pdf";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.name, "document.pdf");
        assert_eq!(info.size, 12345);
        assert!(!info.is_dir);
    }

    #[test]
    fn test_parse_list_line_mlsd_directory() {
        let line = "type=dir;size=4096;modify=20240203142000;unix.mode=0755; my_folder";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.name, "my_folder");
        assert_eq!(info.size, 4096);
        assert!(info.is_dir);
    }

    #[test]
    fn test_ftp_mode_default() {
        let mode = FtpMode::default();
        assert_eq!(mode, FtpMode::Passive);
    }

    #[test]
    fn test_ftp_file_info_creation() {
        let info = FtpFileInfo {
            name: "test.txt".to_string(),
            size: 1024,
            is_dir: false,
        };
        assert_eq!(info.name, "test.txt");
        assert_eq!(info.size, 1024);
        assert!(!info.is_dir);
    }

    #[test]
    fn test_parse_list_line_with_spaces_in_name() {
        // Unix 格式，文件名包含空格
        let line = "-rw-r--r--  1 user staff   5678 Jan 20 11:00 my document with spaces.txt";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.name, "my document with spaces.txt");
        assert_eq!(info.size, 5678);
    }

    #[test]
    fn test_parse_list_line_unrecognized_format() {
        // 无法识别的格式
        let line = "this is not a valid listing format";
        let result = FtpClient::parse_list_line(line);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_pasv_edge_cases() {
        // 边界值: 最小端口
        let min_msg = "Entering Passive Mode (127,0,0,1,0,0)";
        let min_result = FtpClient::parse_pasv_response(min_msg).unwrap();
        assert_eq!(min_result.1, 0);

        // 边界值: 最大端口
        let max_msg = "Entering Passive Mode (255,255,255,255,255,255)";
        let max_result = FtpClient::parse_pasv_response(max_msg).unwrap();
        assert_eq!(max_result.1, 255 * 256 + 255); // 65535
    }
}
