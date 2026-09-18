//! FTP control connection and response handling.

mod commands;
mod data;
mod types;

pub use types::{FtpActiveDataListener, FtpConnection, FtpOptions, FtpResponse, FtpResponseClass};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info};

impl FtpConnection {
    /// Establish a control connection and consume the server welcome response.
    pub async fn connect(
        host: &str,
        port: u16,
        options: Option<FtpOptions>,
    ) -> Result<Self, String> {
        let options = options.unwrap_or_default();
        info!("FTP connecting: {}:{}", host, port);

        let stream = timeout(options.connect_timeout, TcpStream::connect((host, port)))
            .await
            .map_err(|_| {
                format!(
                    "FTP connection timeout ({}s)",
                    options.connect_timeout.as_secs()
                )
            })?
            .map_err(|e| format!("FTP connection failed: {}", e))?;

        let mut conn = Self {
            stream: tokio::io::BufReader::new(stream),
            options,
            host: host.to_string(),
            port,
        };

        let welcome = conn.read_response().await?;
        if !welcome.is_positive_completion() && !welcome.is_positive_preliminary() {
            return Err(format!(
                "FTP server refused connection: {} {}",
                welcome.code, welcome.message
            ));
        }
        debug!("FTP connected: {}", welcome.message);

        Ok(conn)
    }

    async fn send_command(&mut self, command: &str) -> Result<(), String> {
        debug!("FTP command: {}", command.trim());
        self.stream
            .write_all(command.as_bytes())
            .await
            .map_err(|e| format!("Failed to send FTP command: {}", e))?;
        self.stream
            .write_all(b"\r\n")
            .await
            .map_err(|e| format!("Failed to send newline: {}", e))?;
        self.stream
            .flush()
            .await
            .map_err(|e| format!("Failed to flush buffer: {}", e))?;
        Ok(())
    }

    /// Read one FTP response, including multiline responses.
    pub async fn read_response(&mut self) -> Result<FtpResponse, String> {
        let mut line = String::new();
        let mut code: Option<u16> = None;
        let mut message = String::new();
        let mut is_multiline = false;

        loop {
            line.clear();
            let bytes_read = timeout(self.options.read_timeout, self.stream.read_line(&mut line))
                .await
                .map_err(|_| "FTP read timeout".to_string())?
                .map_err(|e| format!("Failed to read FTP response: {}", e))?;

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
        debug!("FTP response: {} {}", code_val, message.trim());
        Ok(FtpResponse {
            code: code_val,
            message,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Duration;

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
        let msg1 = "227 Entering Passive Mode (192,168,1,100,200,10)";
        let result1 = FtpConnection::parse_pasv_response(msg1);
        assert_eq!(result1, Some(("192.168.1.100".to_string(), 200 * 256 + 10)));

        let msg2 = "(10,0,0,1,0,21)";
        let result2 = FtpConnection::parse_pasv_response(msg2);
        assert_eq!(result2, Some(("10.0.0.1".to_string(), 21)));

        let msg3 = "192,168,1,1,195,123";
        let result3 = FtpConnection::parse_pasv_response(msg3);
        assert!(result3.is_none());

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
        let msg1 = "229 |||50001|";
        let result1 = FtpConnection::parse_epsv_response(msg1);
        assert_eq!(result1, Some(50001));

        let msg2 = "Entering Extended Passive Mode (|||60000|)";
        let result2 = FtpConnection::parse_epsv_response(msg2);
        assert_eq!(result2, Some(60000));

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
        let addr = "192.168.1.100";
        let port: u16 = 50001;
        let eprt_cmd = format!("EPRT |1|{}|{}", addr, port);
        assert_eq!(eprt_cmd, "EPRT |1|192.168.1.100|50001");

        let addr_v6 = "::1";
        let eprt_cmd_v6 = format!("EPRT |2|{}|{}", addr_v6, port);
        assert_eq!(eprt_cmd_v6, "EPRT |2|::1|50001");
    }
}
