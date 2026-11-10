//! FTP control-channel sessions over caller-established transports.

use std::time::Duration;
use std::time::SystemTime;
use std::{error::Error, fmt};

use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, BufReader};
use tracing::debug;

use super::control_io::{
    FtpControlReadError, FtpControlWriteError, read_control_line, write_control_command,
};
use super::parsing::{
    cwd_targets, parse_epsv_response, parse_mdtm_timestamp, parse_pasv_response,
    parse_pwd_response, parse_size_response, split_decoded_remote_path,
};

const MAX_RESPONSE_BYTES: usize = 65_536;

#[derive(Debug)]
pub enum FtpControlSessionError {
    Read(FtpControlReadError),
    ReadTimeout(Duration),
    Write(FtpControlWriteError),
    InvalidResponse(String),
    IncompleteResponse,
    ResponseTooLarge(usize),
    Rejected {
        command: &'static str,
        response: FtpControlResponse,
    },
    InvalidValue(String),
}

impl fmt::Display for FtpControlSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => error.fmt(formatter),
            Self::ReadTimeout(timeout) => {
                write!(formatter, "FTP response timeout after {timeout:?}")
            }
            Self::Write(error) => error.fmt(formatter),
            Self::InvalidResponse(message) | Self::InvalidValue(message) => {
                formatter.write_str(message)
            }
            Self::IncompleteResponse => formatter.write_str("incomplete FTP response"),
            Self::ResponseTooLarge(bytes) => {
                write!(formatter, "Max FTP recv buffer reached. length={bytes}")
            }
            Self::Rejected { command, response } => write!(
                formatter,
                "{command} rejected with {} {}",
                response.code, response.message
            ),
        }
    }
}

impl Error for FtpControlSessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::ReadTimeout(_) => None,
            Self::Write(error) => Some(error),
            _ => None,
        }
    }
}

/// Parsed FTP response. Multiline response text is joined with newline bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtpControlResponse {
    pub code: u16,
    pub message: String,
}

/// Stateful command/response interface over an established FTP control stream.
pub struct FtpControlSession<S> {
    reader: BufReader<S>,
}

impl<S: AsyncRead> FtpControlSession<S> {
    pub fn new(stream: S) -> Self {
        Self {
            reader: BufReader::new(stream),
        }
    }

    pub fn get_ref(&self) -> &S {
        self.reader.get_ref()
    }

    pub fn get_mut(&mut self) -> &mut S {
        self.reader.get_mut()
    }

    pub fn reader_mut(&mut self) -> &mut BufReader<S> {
        &mut self.reader
    }

    pub fn into_inner(self) -> S {
        self.reader.into_inner()
    }
}

impl<S> FtpControlSession<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub async fn send_command(&mut self, command: &str) -> Result<(), FtpControlSessionError> {
        debug!(command = %command.trim(), "FTP command");
        write_control_command(self.get_mut(), command)
            .await
            .map_err(FtpControlSessionError::Write)
    }

    pub async fn read_response(
        &mut self,
        timeout: Duration,
    ) -> Result<FtpControlResponse, FtpControlSessionError> {
        read_response(&mut self.reader, timeout).await
    }

    pub async fn command(
        &mut self,
        command: &str,
        timeout: Duration,
    ) -> Result<FtpControlResponse, FtpControlSessionError> {
        self.send_command(command).await?;
        self.read_response(timeout).await
    }

    pub async fn authenticate(
        &mut self,
        username: &str,
        password: &str,
        timeout: Duration,
    ) -> Result<(), FtpControlSessionError> {
        let response = self.command(&format!("USER {username}"), timeout).await?;
        match response.code {
            230 => Ok(()),
            331 | 332 => {
                let response = self.command(&format!("PASS {password}"), timeout).await?;
                if (200..300).contains(&response.code) {
                    Ok(())
                } else {
                    Err(FtpControlSessionError::Rejected {
                        command: "PASS",
                        response,
                    })
                }
            }
            _ => Err(FtpControlSessionError::Rejected {
                command: "USER",
                response,
            }),
        }
    }

    pub async fn set_transfer_type(
        &mut self,
        transfer_type: &str,
        timeout: Duration,
    ) -> Result<(), FtpControlSessionError> {
        let command = if transfer_type.eq_ignore_ascii_case("ascii") {
            "TYPE A"
        } else {
            "TYPE I"
        };
        let response = self.command(command, timeout).await?;
        if response.code == 200 {
            Ok(())
        } else {
            Err(FtpControlSessionError::Rejected { command, response })
        }
    }

    pub async fn prepare_remote_path(
        &mut self,
        remote_path: &str,
        timeout: Duration,
    ) -> Result<String, FtpControlSessionError> {
        let pwd = self.command("PWD", timeout).await?;
        if pwd.code != 257 {
            return Err(FtpControlSessionError::Rejected {
                command: "PWD",
                response: pwd,
            });
        }
        let base = parse_pwd_response(&pwd.message).ok_or_else(|| {
            FtpControlSessionError::InvalidValue(format!(
                "PWD response missing quoted path: {}",
                pwd.message.trim()
            ))
        })?;
        let (directory, file) = split_decoded_remote_path(remote_path);
        for target in cwd_targets(&base, &directory) {
            let response = self.command(&format!("CWD {target}"), timeout).await?;
            if response.code != 250 {
                return Err(FtpControlSessionError::Rejected {
                    command: "CWD",
                    response,
                });
            }
        }
        Ok(file)
    }

    pub async fn modification_time(
        &mut self,
        remote_path: &str,
        timeout: Duration,
    ) -> Result<Option<SystemTime>, FtpControlSessionError> {
        let response = self
            .command(&format!("MDTM {remote_path}"), timeout)
            .await?;
        if response.code != 213 {
            return Ok(None);
        }
        Ok(response
            .message
            .trim()
            .get(..14)
            .and_then(parse_mdtm_timestamp))
    }

    pub async fn file_size(
        &mut self,
        remote_path: &str,
        timeout: Duration,
    ) -> Result<Option<u64>, FtpControlSessionError> {
        let response = self
            .command(&format!("SIZE {remote_path}"), timeout)
            .await?;
        if response.code == 550 {
            return Err(FtpControlSessionError::Rejected {
                command: "SIZE",
                response,
            });
        }
        if response.code != 213 {
            return Ok(None);
        }
        let size =
            parse_size_response(&response.message).map_err(FtpControlSessionError::InvalidValue)?;
        Ok(Some(size))
    }

    pub async fn passive_data_port(
        &mut self,
        timeout: Duration,
    ) -> Result<u16, FtpControlSessionError> {
        if let Ok(response) = self.command("EPSV", timeout).await {
            if response.code == 229 {
                if let Some(port) = parse_epsv_response(&response.message) {
                    return Ok(port);
                }
            }
        }
        let response = self.command("PASV", timeout).await?;
        if response.code != 227 {
            return Err(FtpControlSessionError::Rejected {
                command: "PASV",
                response,
            });
        }
        parse_pasv_response(&response.message)
            .map(|(_, port)| port)
            .ok_or_else(|| {
                FtpControlSessionError::InvalidResponse("Cannot parse PASV response".into())
            })
    }

    pub async fn advertise_active_endpoint(
        &mut self,
        address: std::net::SocketAddr,
        timeout: Duration,
    ) -> Result<(), FtpControlSessionError> {
        let ip = address.ip();
        let eprt = format!(
            "EPRT |{}|{}|{}|",
            if ip.is_ipv4() { 1 } else { 2 },
            ip,
            address.port()
        );
        let response = self.command(&eprt, timeout).await?;
        if (200..300).contains(&response.code) {
            return Ok(());
        }
        if let std::net::IpAddr::V4(ip) = ip {
            let octets = ip.octets();
            let port = address.port();
            let command = format!(
                "PORT {},{},{},{},{},{}",
                octets[0],
                octets[1],
                octets[2],
                octets[3],
                port / 256,
                port % 256
            );
            let response = self.command(&command, timeout).await?;
            if (200..300).contains(&response.code) {
                return Ok(());
            }
            return Err(FtpControlSessionError::Rejected {
                command: "PORT",
                response,
            });
        }
        Err(FtpControlSessionError::Rejected {
            command: "EPRT",
            response,
        })
    }

    pub async fn set_restart_offset(
        &mut self,
        offset: u64,
        timeout: Duration,
    ) -> Result<bool, FtpControlSessionError> {
        let response = self.command(&format!("REST {offset}"), timeout).await?;
        Ok(response.code == 350 || offset == 0)
    }

    pub async fn retrieve(
        &mut self,
        remote_path: &str,
        timeout: Duration,
    ) -> Result<(), FtpControlSessionError> {
        let response = self
            .command(&format!("RETR {remote_path}"), timeout)
            .await?;
        if matches!(response.code, 125 | 150) {
            Ok(())
        } else {
            Err(FtpControlSessionError::Rejected {
                command: "RETR",
                response,
            })
        }
    }

    pub async fn abort(&mut self, timeout: Duration) {
        let _ = self.command("ABOR", timeout).await;
    }

    pub async fn quit(&mut self, timeout: Duration) {
        let _ = self.command("QUIT", timeout).await;
    }
}

/// Read and parse one RFC 959 response, including its multiline terminator.
/// The timeout applies independently to each response line.
pub async fn read_response<R>(
    reader: &mut R,
    timeout: Duration,
) -> Result<FtpControlResponse, FtpControlSessionError>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = String::new();
    let mut code = None;
    let mut message = String::new();
    let mut multiline = false;
    let mut total_bytes = 0;

    loop {
        line.clear();
        let bytes_read = read_control_line(reader, &mut line, timeout)
            .await
            .map_err(|error| match error {
                FtpControlReadError::Timeout => FtpControlSessionError::ReadTimeout(timeout),
                FtpControlReadError::Io(error) => {
                    FtpControlSessionError::Read(FtpControlReadError::Io(error))
                }
            })?;
        if bytes_read == 0 || !line.ends_with("\r\n") {
            return Err(FtpControlSessionError::IncompleteResponse);
        }
        total_bytes += bytes_read;
        if total_bytes > MAX_RESPONSE_BYTES {
            return Err(FtpControlSessionError::ResponseTooLarge(total_bytes));
        }

        let line = &line[..line.len() - 2];
        if code.is_none() {
            let (parsed_code, is_multiline, text) = parse_status_line(line)?;
            code = Some(parsed_code);
            multiline = is_multiline;
            message.push_str(text);
            if multiline {
                message.push('\n');
                continue;
            }
            break;
        }

        if multiline {
            let expected_prefix = format!("{} ", code.expect("multiline response has a code"));
            if let Some(text) = line.strip_prefix(&expected_prefix) {
                message.push_str(text);
                break;
            }
            message.push_str(line);
            message.push('\n');
        } else {
            break;
        }
    }

    let code = code.ok_or(FtpControlSessionError::IncompleteResponse)?;
    debug!(code, message = %message.trim(), "FTP response");
    Ok(FtpControlResponse { code, message })
}

fn parse_status_line(line: &str) -> Result<(u16, bool, &str), FtpControlSessionError> {
    let bytes = line.as_bytes();
    if bytes.len() < 4 || !bytes[..3].iter().all(u8::is_ascii_digit) {
        return Err(FtpControlSessionError::InvalidResponse(line.to_owned()));
    }
    if !matches!(bytes[3], b' ' | b'-') {
        return Err(FtpControlSessionError::InvalidResponse(line.to_owned()));
    }
    let code = line[..3]
        .parse::<u16>()
        .map_err(|_| FtpControlSessionError::InvalidResponse(line.to_owned()))?;
    Ok((code, bytes[3] == b'-', &line[4..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn parses_multiline_responses() {
        let bytes = b"211-Features:\r\n UTF8\r\n211 End\r\n";
        let mut reader = BufReader::new(std::io::Cursor::new(bytes));
        let response = read_response(&mut reader, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(response.code, 211);
        assert_eq!(response.message, "Features:\n UTF8\nEnd");
    }

    #[tokio::test]
    async fn rejects_malformed_and_unterminated_responses() {
        let mut malformed = BufReader::new(std::io::Cursor::new(b"20x invalid\r\n"));
        assert!(matches!(
            read_response(&mut malformed, Duration::from_secs(1)).await,
            Err(FtpControlSessionError::InvalidResponse(_))
        ));

        let mut incomplete = BufReader::new(std::io::Cursor::new(b"220 welcome"));
        assert!(matches!(
            read_response(&mut incomplete, Duration::from_secs(1)).await,
            Err(FtpControlSessionError::IncompleteResponse)
        ));
    }

    #[tokio::test]
    async fn performs_download_control_sequence_over_injected_stream() {
        let (client, mut server) = tokio::io::duplex(4096);
        let server_task = tokio::spawn(async move {
            let responses = [
                ("USER test", "331 Password required\r\n"),
                ("PASS secret", "230 Logged in\r\n"),
                ("TYPE I", "200 Binary\r\n"),
                ("PWD", "257 \"/\" current\r\n"),
                ("CWD /", "250 Root directory\r\n"),
                ("CWD pub", "250 Directory changed\r\n"),
                ("MDTM file.bin", "213 20240115103000\r\n"),
                ("SIZE file.bin", "213 42\r\n"),
                ("EPSV", "229 Entering Extended Passive Mode (|||50001|)\r\n"),
                ("REST 8", "350 Restart accepted\r\n"),
                ("RETR file.bin", "150 Opening data\r\n"),
            ];
            let mut reader = BufReader::new(&mut server);
            let mut line = String::new();
            for (expected_command, response) in responses {
                line.clear();
                reader.read_line(&mut line).await.unwrap();
                assert_eq!(line.trim_end(), expected_command);
                reader
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            }
        });

        let mut session = FtpControlSession::new(client);
        let timeout = Duration::from_secs(1);
        session
            .authenticate("test", "secret", timeout)
            .await
            .unwrap();
        session.set_transfer_type("binary", timeout).await.unwrap();
        assert_eq!(
            session
                .prepare_remote_path("/pub/file.bin", timeout)
                .await
                .unwrap(),
            "file.bin"
        );
        assert!(
            session
                .modification_time("file.bin", timeout)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            session.file_size("file.bin", timeout).await.unwrap(),
            Some(42)
        );
        assert_eq!(session.passive_data_port(timeout).await.unwrap(), 50001);
        assert!(session.set_restart_offset(8, timeout).await.unwrap());
        session.retrieve("file.bin", timeout).await.unwrap();
        server_task.await.unwrap();
    }
}
