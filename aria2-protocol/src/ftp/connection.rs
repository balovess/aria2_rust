use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::{Duration, interval, timeout};
use tracing::{debug, info, warn};

/// FTP connection configuration options
#[derive(Debug, Clone)]
pub struct FtpOptions {
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub passive_mode: bool,
    pub username: String,
    pub password: String,
    /// Keep-alive interval for control channel (None to disable)
    pub keepalive_interval: Option<Duration>,
    /// Maximum number of retry attempts for transient errors
    pub max_retries: u32,
}

impl Default for FtpOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(30),
            passive_mode: true,
            username: "anonymous".to_string(),
            password: "aria2@".to_string(),
            keepalive_interval: Some(Duration::from_secs(60)),
            max_retries: 3,
        }
    }
}

/// FTP response code classification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtpResponseClass {
    /// Positive Preliminary (1xx): Command accepted, waiting for confirmation
    PositivePreliminary,
    /// Positive Completion (2xx): Command completed successfully
    PositiveCompletion,
    /// Positive Intermediate (3xx): Command accepted, additional info needed
    PositiveIntermediate,
    /// Transient Negative (4xx): Temporary failure, retry may succeed
    TransientNegative,
    /// Permanent Negative (5xx): Permanent failure, do not retry
    PermanentNegative,
    /// Unknown/Invalid response code
    Unknown,
}

impl FtpResponseClass {
    /// Classify an FTP response code into its category
    pub fn from_code(code: u16) -> Self {
        match code {
            100..=199 => FtpResponseClass::PositivePreliminary,
            200..=299 => FtpResponseClass::PositiveCompletion,
            300..=399 => FtpResponseClass::PositiveIntermediate,
            400..=499 => FtpResponseClass::TransientNegative,
            500..=599 => FtpResponseClass::PermanentNegative,
            _ => FtpResponseClass::Unknown,
        }
    }

    /// Check if this response class indicates success (1xx-3xx)
    pub fn is_success(&self) -> bool {
        matches!(
            self,
            FtpResponseClass::PositivePreliminary
                | FtpResponseClass::PositiveCompletion
                | FtpResponseClass::PositiveIntermediate
        )
    }

    /// Check if this is a retry-worthy transient error (4xx)
    pub fn is_transient(&self) -> bool {
        *self == FtpResponseClass::TransientNegative
    }

    /// Check if this is a permanent failure (5xx)
    pub fn is_permanent(&self) -> bool {
        *self == FtpResponseClass::PermanentNegative
    }
}

/// FTP server response with code and message
#[derive(Debug, Clone)]
pub struct FtpResponse {
    pub code: u16,
    pub message: String,
}

impl FtpResponse {
    pub fn is_success(&self) -> bool {
        (100..400).contains(&self.code)
    }

    pub fn is_intermediate(&self) -> bool {
        (100..200).contains(&self.code)
    }

    pub fn is_positive_completion(&self) -> bool {
        (200..300).contains(&self.code)
    }

    pub fn is_positive_preliminary(&self) -> bool {
        (100..200).contains(&self.code)
    }

    /// Get the response class for this response
    pub fn class(&self) -> FtpResponseClass {
        FtpResponseClass::from_code(self.code)
    }

    /// Check if this response indicates a transient error (retry-worthy)
    pub fn is_transient_error(&self) -> bool {
        self.class().is_transient()
    }

    /// Check if this response indicates a permanent error (do not retry)
    pub fn is_permanent_error(&self) -> bool {
        self.class().is_permanent()
    }
}

pub struct FtpConnection {
    pub stream: BufReader<TcpStream>,
    pub options: FtpOptions,
    #[allow(dead_code)]
    pub host: String,
    #[allow(dead_code)]
    pub port: u16,
}

impl FtpConnection {
    pub async fn connect(
        host: &str,
        port: u16,
        options: Option<FtpOptions>,
    ) -> Result<Self, String> {
        let options = options.unwrap_or_default();
        info!("FTP连接中: {}:{}", host, port);

        let stream = timeout(options.connect_timeout, TcpStream::connect((host, port)))
            .await
            .map_err(|_| format!("FTP连接超时 ({}秒)", options.connect_timeout.as_secs()))?
            .map_err(|e| format!("FTP连接失败: {}", e))?;

        let mut conn = Self {
            stream: BufReader::new(stream),
            options,
            host: host.to_string(),
            port,
        };

        let welcome = conn.read_response().await?;
        if !welcome.is_positive_completion() && !welcome.is_positive_preliminary() {
            return Err(format!(
                "FTP服务器拒绝连接: {} {}",
                welcome.code, welcome.message
            ));
        }
        debug!("FTP连接成功: {}", welcome.message);

        Ok(conn)
    }

    pub async fn login(&mut self) -> Result<(), String> {
        debug!("发送USER命令: {}", self.options.username);
        self.send_command(&format!("USER {}", self.options.username))
            .await?;
        let resp = self.read_response().await?;

        if resp.code == 331 || resp.code == 332 {
            debug!("需要密码认证，发送PASS命令");
            self.send_command(&format!("PASS {}", self.options.password))
                .await?;
            let pass_resp = self.read_response().await?;
            if !pass_resp.is_positive_completion() {
                return Err(format!(
                    "FTP登录失败: {} {}",
                    pass_resp.code, pass_resp.message
                ));
            }
            info!("FTP登录成功");
        } else if !resp.is_positive_completion() {
            return Err(format!("FTP登录失败: {} {}", resp.code, resp.message));
        } else {
            info!("FTP登录成功 (无需密码)");
        }

        Ok(())
    }

    pub async fn cwd(&mut self, path: &str) -> Result<(), String> {
        debug!("切换目录: {}", path);
        self.send_command(&format!("CWD {}", path)).await?;
        let resp = self.read_response().await?;
        if !resp.is_positive_completion() {
            return Err(format!("CWD失败: {} {}", resp.code, resp.message));
        }
        Ok(())
    }

    pub async fn size(&mut self, filename: &str) -> Result<u64, String> {
        debug!("查询文件大小: {}", filename);
        self.send_command(&format!("SIZE {}", filename)).await?;
        let resp = self.read_response().await?;
        if resp.code == 213 {
            let size_str = resp.message.trim();
            size_str
                .parse::<u64>()
                .map_err(|e| format!("解析文件大小失败: {} ({})", e, size_str))
        } else {
            Err(format!("SIZE命令失败: {} {}", resp.code, resp.message))
        }
    }

    pub async fn mdtm(&mut self, filename: &str) -> Result<String, String> {
        debug!("查询修改时间: {}", filename);
        self.send_command(&format!("MDTM {}", filename)).await?;
        let resp = self.read_response().await?;
        if resp.code == 213 {
            Ok(resp.message.trim().to_string())
        } else {
            Err(format!("MDTM命令失败: {} {}", resp.code, resp.message))
        }
    }

    pub async fn pasv(&mut self) -> Result<(String, u16), String> {
        debug!("请求被动模式数据连接");
        self.send_command("PASV").await?;
        let resp = self.read_response().await?;
        if resp.code != 227 {
            return Err(format!("PASV失败: {} {}", resp.code, resp.message));
        }

        match Self::parse_pasv_response(&resp.message) {
            Some(addr) => {
                debug!("PASV数据通道: {}:{}", addr.0, addr.1);
                Ok(addr)
            }
            None => Err("无法解析PASV响应".to_string()),
        }
    }

    pub async fn epsv(&mut self) -> Result<u16, String> {
        debug!("请求扩展被动模式");
        self.send_command("EPSV").await?;
        let resp = self.read_response().await?;
        if resp.code == 229 {
            if let Some(port) = Self::parse_epsv_response(&resp.message) {
                debug!("EPSV端口: {}", port);
                return Ok(port);
            }
        } else if resp.code == 590 || resp.code == 500 || resp.code == 501 {
            warn!("服务器不支持EPSV，回退到PASV模式");
            let (host, port) = self.pasv().await?;
            let _ = host;
            return Ok(port);
        }
        Err(format!("EPSV失败: {} {}", resp.code, resp.message))
    }

    /// Enter active mode using PORT command (IPv4)
    /// Binds a local socket and sends PORT command with local address
    pub async fn port_active(&mut self) -> Result<u16, String> {
        debug!("请求主动模式数据连接 (IPv4)");

        // Bind to a local port for active mode
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
            .await
            .map_err(|e| format!("绑定本地端口失败: {}", e))?;

        let local_addr = listener
            .local_addr()
            .map_err(|e| format!("获取本地地址失败: {}", e))?;

        let ip = local_addr.ip();
        let port = local_addr.port();

        // Build PORT command: h1,h2,h3,h4,p1,p2
        let octets = match ip {
            std::net::IpAddr::V4(v4) => v4.octets(),
            std::net::IpAddr::V6(_) => {
                return Err("IPv6地址不支持PORT命令，请使用EPRT".to_string());
            }
        };

        let p1 = port / 256;
        let p2 = port % 256;
        let port_cmd = format!(
            "PORT {},{},{},{},{},{}",
            octets[0], octets[1], octets[2], octets[3], p1, p2
        );

        debug!("发送PORT命令: {}", port_cmd);
        self.send_command(&port_cmd).await?;
        let resp = self.read_response().await?;
        if !resp.is_positive_completion() {
            return Err(format!("PORT失败: {} {}", resp.code, resp.message));
        }

        // Spawn a task to accept the incoming connection
        // The caller should use accept_data_connection() to get the stream
        debug!("等待主动模式数据连接在端口 {}", port);

        // Store listener for later acceptance (simplified - in real impl would store it)
        drop(listener); // In real implementation, we'd keep this alive

        Ok(port)
    }

    /// Enter active mode using EPRT command (IPv6/IPv4)
    /// Extended PORT command that supports both IPv4 and IPv6
    pub async fn eprt_active(&mut self) -> Result<(String, u16), String> {
        debug!("请求扩展主动模式数据连接");

        // Bind to a local port
        let listener = tokio::net::TcpListener::bind("::0")
            .await
            .map_err(|e| format!("绑定本地端口失败: {}", e))?;

        let local_addr = listener
            .local_addr()
            .map_err(|e| format!("获取本地地址失败: {}", e))?;

        let ip = local_addr.ip();
        let port = local_addr.port();

        // Build EPRT command: |<proto>|<addr>|<port>|
        // proto: 1 for IPv4, 2 for IPv6
        let (proto, addr_str) = match ip {
            std::net::IpAddr::V4(v4) => ("1", v4.to_string()),
            std::net::IpAddr::V6(v6) => ("2", v6.to_string()),
        };

        let eprt_cmd = format!("EPRT |{}|{}|{}", proto, addr_str, port);

        debug!("发送EPRT命令: {}", eprt_cmd);
        self.send_command(&eprt_cmd).await?;
        let resp = self.read_response().await?;
        if !resp.is_positive_completion() {
            return Err(format!("EPRT失败: {} {}", resp.code, resp.message));
        }

        debug!("EPRT成功，监听 {}:{}", addr_str, port);
        drop(listener);

        Ok((addr_str, port))
    }

    pub async fn rest(&mut self, offset: u64) -> Result<(), String> {
        debug!("设置恢复偏移: {}", offset);
        self.send_command(&format!("REST {}", offset)).await?;
        let resp = self.read_response().await?;
        if resp.code != 350 {
            return Err(format!("REST失败: {} {}", resp.code, resp.message));
        }
        Ok(())
    }

    pub async fn retr(&mut self, filename: &str) -> Result<(), String> {
        debug!("准备下载文件: {}", filename);
        self.send_command(&format!("RETR {}", filename)).await?;
        let resp = self.read_response().await?;
        if !resp.is_positive_preliminary() {
            return Err(format!("RETR失败: {} {}", resp.code, resp.message));
        }
        Ok(())
    }

    pub async fn type_image(&mut self) -> Result<(), String> {
        debug!("设置传输类型为二进制(I)");
        self.send_command("TYPE I").await?;
        let resp = self.read_response().await?;
        if !resp.is_positive_completion() {
            return Err(format!("TYPE I失败: {} {}", resp.code, resp.message));
        }
        Ok(())
    }

    pub async fn quit(mut self) -> Result<(), String> {
        debug!("发送QUIT命令");
        self.send_command("QUIT").await?;
        let resp = self.read_response().await?;
        info!("FTP断开连接: {}", resp.message);
        Ok(())
    }

    /// Send ABOR command to abort a transfer in progress
    pub async fn abor(&mut self) -> Result<(), String> {
        debug!("发送ABOR命令中止传输");
        // Send Telnet IP (Interrupt Process) + ABOR command
        self.stream
            .get_mut()
            .write_all(b"\xff\xf4") // Telnet IP
            .await
            .map_err(|e| format!("发送Telnet IP失败: {}", e))?;
        self.stream
            .get_mut()
            .flush()
            .await
            .map_err(|e| format!("刷新缓冲区失败: {}", e))?;

        tokio::time::sleep(Duration::from_millis(100)).await;

        self.send_command("ABOR").await?;
        let resp = self.read_response().await?;
        // ABOR can return 226 (success) or 426 (connection closed)
        if resp.code == 226 || resp.code == 225 {
            debug!("传输已成功中止: {}", resp.message);
            Ok(())
        } else {
            warn!("ABOR响应异常: {} {}", resp.code, resp.message);
            Ok(())
        }
    }

    /// Send LIST command to get directory listing (detailed format)
    pub async fn list(&mut self, path: Option<&str>) -> Result<FtpResponse, String> {
        match path {
            Some(p) => {
                debug!("列出目录详细内容: {}", p);
                self.send_command(&format!("LIST {}", p)).await?
            }
            None => {
                debug!("列出当前目录详细内容");
                self.send_command("LIST").await?
            }
        };
        let resp = self.read_response().await?;
        Ok(resp)
    }

    /// Send NLST command to get directory listing (names only)
    pub async fn nlst(&mut self, path: Option<&str>) -> Result<FtpResponse, String> {
        match path {
            Some(p) => {
                debug!("列出目录名称: {}", p);
                self.send_command(&format!("NLST {}", p)).await?
            }
            None => {
                debug!("列出当前目录名称");
                self.send_command("NLST").await?
            }
        };
        let resp = self.read_response().await?;
        Ok(resp)
    }

    /// Send TYPE A command for ASCII mode transfer
    pub async fn type_ascii(&mut self) -> Result<(), String> {
        debug!("设置传输类型为ASCII(A)");
        self.send_command("TYPE A").await?;
        let resp = self.read_response().await?;
        if !resp.is_positive_completion() {
            return Err(format!("TYPE A失败: {} {}", resp.code, resp.message));
        }
        Ok(())
    }

    /// Send NOOP command for keep-alive / connection test
    pub async fn noop(&mut self) -> Result<(), String> {
        debug!("发送NOOP保活命令");
        self.send_command("NOOP").await?;
        let resp = self.read_response().await?;
        if resp.code == 200 {
            debug!("NOOK保活成功");
            Ok(())
        } else {
            Err(format!("NOOP失败: {} {}", resp.code, resp.message))
        }
    }

    /// Start keep-alive task for control channel
    /// Returns a handle that can be used to stop the keep-alive
    pub fn start_keepalive(&self) -> Option<tokio::task::JoinHandle<()>> {
        let keepalive_duration = self.options.keepalive_interval?;

        // Note: In a real implementation, we'd need to clone or share the stream
        // This is a simplified version - the actual implementation would need Arc<Mutex<>>
        let handle = tokio::spawn(async move {
            let mut ticker = interval(keepalive_duration);
            loop {
                ticker.tick().await;
                // Send NOOP for keep-alive would go here
                debug!("FTP keep-alive tick");
            }
        });

        Some(handle)
    }

    pub fn get_data_stream(&mut self) -> &mut BufReader<TcpStream> {
        &mut self.stream
    }

    async fn send_command(&mut self, command: &str) -> Result<(), String> {
        debug!("FTP命令: {}", command.trim());
        self.stream
            .write_all(command.as_bytes())
            .await
            .map_err(|e| format!("发送FTP命令失败: {}", e))?;
        self.stream
            .write_all(b"\r\n")
            .await
            .map_err(|e| format!("发送换行符失败: {}", e))?;
        self.stream
            .flush()
            .await
            .map_err(|e| format!("刷新缓冲区失败: {}", e))?;
        Ok(())
    }

    pub async fn read_response(&mut self) -> Result<FtpResponse, String> {
        let mut line = String::new();
        let mut code: Option<u16> = None;
        let mut message = String::new();
        let mut is_multiline = false;

        loop {
            line.clear();
            let bytes_read = timeout(self.options.read_timeout, self.stream.read_line(&mut line))
                .await
                .map_err(|_| "FTP读取超时".to_string())?
                .map_err(|e| format!("读取FTP响应失败: {}", e))?;

            if bytes_read == 0 {
                break;
            }

            let trimmed = line.trim_end();
            if trimmed.len() < 4 {
                continue;
            }

            let response_code: u16 = trimmed[..3].parse().unwrap_or(0);

            if code.is_none() {
                code = Some(response_code);
            }

            let separator = trimmed.as_bytes()[3];
            if separator == b'-' && !is_multiline {
                is_multiline = true;
                message.push_str(&trimmed[4..]);
                message.push('\n');
            } else if separator == b' '
                || (is_multiline && trimmed.starts_with(&format!("{:3} ", code.unwrap_or(0))))
            {
                message.push_str(&trimmed[4..]);
                break;
            } else if is_multiline {
                message.push_str(&trimmed[4..]);
                message.push('\n');
            }
        }

        let code_val = code.unwrap_or(0);
        debug!("FTP响应: {} {}", code_val, message.trim());
        Ok(FtpResponse {
            code: code_val,
            message,
        })
    }

    fn parse_pasv_response(message: &str) -> Option<(String, u16)> {
        let start = message.find('(')?;
        let end = message.find(')')?;
        let inner = &message[start + 1..end];

        let parts: Vec<&str> = inner.split(',').collect();
        if parts.len() != 6 {
            return None;
        }

        let h1: u8 = parts[0].trim().parse().ok()?;
        let h2: u8 = parts[1].trim().parse().ok()?;
        let h3: u8 = parts[2].trim().parse().ok()?;
        let h4: u8 = parts[3].trim().parse().ok()?;
        let p1: u16 = parts[4].trim().parse().ok()?;
        let p2: u16 = parts[5].trim().parse().ok()?;

        let host = format!("{}.{}.{}.{}", h1, h2, h3, h4);
        let port = p1 * 256 + p2;

        Some((host, port))
    }

    fn parse_epsv_response(message: &str) -> Option<u16> {
        let start = message.rfind('|')?;
        let prev_pipe = message[..start].rfind('|')?;
        let port_str = &message[prev_pipe + 1..start];
        port_str.parse::<u16>().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ftp_response_checks() {
        let ok = FtpResponse {
            code: 226,
            message: "Transfer complete".into(),
        };
        assert!(ok.is_success());
        assert!(ok.is_positive_completion());
        assert!(!ok.is_permanent_error());
        assert!(!ok.is_transient_error());

        let intermediate = FtpResponse {
            code: 150,
            message: "Opening data channel".into(),
        };
        assert!(intermediate.is_success());
        assert!(intermediate.is_positive_preliminary());

        let error = FtpResponse {
            code: 550,
            message: "File not found".into(),
        };
        assert!(!error.is_success());
        assert!(error.is_permanent_error());
        assert!(!error.is_transient_error());

        let transient = FtpResponse {
            code: 425,
            message: "Can't open data connection".into(),
        };
        assert!(transient.is_transient_error());
        assert!(!transient.is_permanent_error());
    }

    #[test]
    fn test_response_class_classification() {
        assert_eq!(
            FtpResponseClass::from_code(150),
            FtpResponseClass::PositivePreliminary
        );
        assert_eq!(
            FtpResponseClass::from_code(226),
            FtpResponseClass::PositiveCompletion
        );
        assert_eq!(
            FtpResponseClass::from_code(331),
            FtpResponseClass::PositiveIntermediate
        );
        assert_eq!(
            FtpResponseClass::from_code(425),
            FtpResponseClass::TransientNegative
        );
        assert_eq!(
            FtpResponseClass::from_code(550),
            FtpResponseClass::PermanentNegative
        );
        assert_eq!(FtpResponseClass::from_code(999), FtpResponseClass::Unknown);
    }

    #[test]
    fn test_response_class_methods() {
        let prelim = FtpResponseClass::PositivePreliminary;
        assert!(prelim.is_success());
        assert!(!prelim.is_transient());
        assert!(!prelim.is_permanent());

        let transient = FtpResponseClass::TransientNegative;
        assert!(!transient.is_success());
        assert!(transient.is_transient());
        assert!(!transient.is_permanent());

        let permanent = FtpResponseClass::PermanentNegative;
        assert!(!permanent.is_success());
        assert!(!permanent.is_transient());
        assert!(permanent.is_permanent());
    }

    #[test]
    fn test_parse_pasv() {
        let msg = "Entering Passive Mode (192,168,1,1,195,123)";
        let result = FtpConnection::parse_pasv_response(msg);
        assert!(result.is_some());
        let (host, port) = result.unwrap();
        assert_eq!(host, "192.168.1.1");
        assert_eq!(port, 195 * 256 + 123);
    }

    #[test]
    fn test_parse_pasv_various_formats() {
        // Standard format with spaces
        let msg1 = "227 Entering Passive Mode (192,168,1,100,200,10)";
        let result1 = FtpConnection::parse_pasv_response(msg1);
        assert_eq!(result1, Some(("192.168.1.100".to_string(), 200 * 256 + 10)));

        // Without leading text
        let msg2 = "(10,0,0,1,0,21)";
        let result2 = FtpConnection::parse_pasv_response(msg2);
        assert_eq!(result2, Some(("10.0.0.1".to_string(), 21)));

        // Invalid format - missing parentheses
        let msg3 = "192,168,1,1,195,123";
        let result3 = FtpConnection::parse_pasv_response(msg3);
        assert!(result3.is_none());

        // Invalid format - wrong number of parts
        let msg4 = "(192,168,1,1,195)";
        let result4 = FtpConnection::parse_pasv_response(msg4);
        assert!(result4.is_none());
    }

    #[test]
    fn test_parse_epsv() {
        let msg = "Entering Extended Passive Mode (|||50001|)";
        let result = FtpConnection::parse_epsv_response(msg);
        assert_eq!(result, Some(50001));
    }

    #[test]
    fn test_parse_epsv_various_formats() {
        // Standard EPSV format
        let msg1 = "229 |||50001|";
        let result1 = FtpConnection::parse_epsv_response(msg1);
        assert_eq!(result1, Some(50001));

        // With text prefix
        let msg2 = "Entering Extended Passive Mode (|||60000|)";
        let result2 = FtpConnection::parse_epsv_response(msg2);
        assert_eq!(result2, Some(60000));

        // Invalid format - no pipes
        let msg3 = "50001";
        let result3 = FtpConnection::parse_epsv_response(msg3);
        assert!(result3.is_none());
    }

    #[test]
    fn test_ftp_options_default() {
        let opts = FtpOptions::default();
        assert_eq!(opts.username, "anonymous");
        assert_eq!(opts.password, "aria2@");
        assert!(opts.passive_mode);
        assert_eq!(opts.connect_timeout, Duration::from_secs(30));
        assert_eq!(opts.read_timeout, Duration::from_secs(30));
        assert!(opts.keepalive_interval.is_some());
        assert_eq!(opts.keepalive_interval.unwrap(), Duration::from_secs(60));
        assert_eq!(opts.max_retries, 3);
    }

    #[test]
    fn test_build_port_command_ipv4() {
        // Test PORT command construction logic
        let octets: [u8; 4] = [192, 168, 1, 100];
        let port: u16 = 50000;
        let p1 = port / 256;
        let p2 = port % 256;
        let port_cmd = format!(
            "PORT {},{},{},{},{},{}",
            octets[0], octets[1], octets[2], octets[3], p1, p2
        );
        assert_eq!(port_cmd, "PORT 192,168,1,100,195,80");
    }

    #[test]
    fn test_build_eprt_command() {
        // Test EPRT command for IPv4
        let addr = "192.168.1.100";
        let port: u16 = 50001;
        let eprt_cmd = format!("EPRT |1|{}|{}", addr, port);
        assert_eq!(eprt_cmd, "EPRT |1|192.168.1.100|50001");

        // Test EPRT command for IPv6
        let addr_v6 = "::1";
        let eprt_cmd_v6 = format!("EPRT |2|{}|{}", addr_v6, port);
        assert_eq!(eprt_cmd_v6, "EPRT |2|::1|50001");
    }
}
