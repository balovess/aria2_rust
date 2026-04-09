use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};
use tracing::{debug, info, warn};

#[derive(Debug, Clone)]
pub struct FtpOptions {
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub passive_mode: bool,
    pub username: String,
    pub password: String,
}

impl Default for FtpOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(30),
            passive_mode: true,
            username: "anonymous".to_string(),
            password: "aria2@".to_string(),
        }
    }
}

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
}

pub struct FtpConnection {
    pub stream: BufReader<TcpStream>,
    pub options: FtpOptions,
    #[allow(dead_code)]
    host: String,
    #[allow(dead_code)]
    port: u16,
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
        } else if resp.code == 590 {
            warn!("服务器不支持EPSV，回退到PASV模式");
            let (host, port) = self.pasv().await?;
            let _ = host;
            return Ok(port);
        }
        Err(format!("EPSV失败: {} {}", resp.code, resp.message))
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
                .map_err(|_| "FTP 读取超时".to_string())?
                .map_err(|e| format!("读取 FTP 响应失败：{}", e))?;

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
            } else if separator == b' ' {
                message.push_str(&trimmed[4..]);
                break;
            } else if is_multiline && trimmed.starts_with(&format!("{:3} ", code.unwrap_or(0))) {
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
    fn test_parse_epsv() {
        let msg = "Entering Extended Passive Mode (|||50001|)";
        let result = FtpConnection::parse_epsv_response(msg);
        assert_eq!(result, Some(50001));
    }

    #[test]
    fn test_ftp_options_default() {
        let opts = FtpOptions::default();
        assert_eq!(opts.username, "anonymous");
        assert_eq!(opts.password, "aria2@");
        assert!(opts.passive_mode);
    }
}
