use std::time::Duration;

use tokio::io::AsyncBufRead;
use tokio::time::timeout;
use tracing::debug;

use crate::error::{Aria2Error, RecoverableError, Result};

/// Maximum receive buffer size for FTP control responses (64KB).
const MAX_RECV_BUFFER: usize = 65536;

/// Read one complete FTP response, including RFC 959 multiline responses.
pub(crate) async fn read_response_impl<R>(
    reader: &mut R,
    timeout_dur: Duration,
) -> Result<(u16, String)>
where
    R: AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;

    let mut line = String::new();
    let mut code: Option<u16> = None;
    let mut message = String::new();
    let mut is_multiline = false;
    let mut total_bytes = 0;

    loop {
        line.clear();
        let bytes_read = timeout(timeout_dur, reader.read_line(&mut line))
            .await
            .map_err(|_| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("FTP response timeout after {timeout_dur:?}"),
                })
            })?
            .map_err(|error| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("FTP read response error: {error}"),
                })
            })?;

        if bytes_read == 0 || !line.ends_with("\r\n") {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "FTP response ended before CRLF-terminated response was complete"
                        .into(),
                },
            ));
        }

        total_bytes += bytes_read;
        if total_bytes > MAX_RECV_BUFFER {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!("Max FTP recv buffer reached. length={total_bytes}"),
                },
            ));
        }

        let line = &line[..line.len() - 2];
        if code.is_none() {
            let status = parse_status_line(line)?;
            code = Some(status.0);
            is_multiline = status.1;

            if status.1 {
                message.push_str(status.2);
                message.push('\n');
                continue;
            }

            message.push_str(status.2);
            break;
        }

        if is_multiline {
            let expected_prefix = format!("{} ", code.expect("multiline response has a code"));
            if line.starts_with(&expected_prefix) {
                message.push_str(&line[expected_prefix.len()..]);
                break;
            }

            message.push_str(line);
            message.push('\n');
            continue;
        }

        break;
    }

    let code = code.ok_or_else(|| {
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: "FTP response ended before a status code was received".into(),
        })
    })?;
    debug!("FTP RESP: {} {}", code, message.trim());
    Ok((code, message))
}

fn parse_status_line(line: &str) -> Result<(u16, bool, &str)> {
    if line.len() < 4 {
        return Err(Aria2Error::Recoverable(
            RecoverableError::FtpProtocolError {
                message: format!("Invalid FTP response line: {line:?}"),
            },
        ));
    }

    let bytes = line.as_bytes();
    if !bytes[..3].iter().all(u8::is_ascii_digit) || !matches!(bytes[3], b' ' | b'-') {
        return Err(Aria2Error::Recoverable(
            RecoverableError::FtpProtocolError {
                message: format!("Invalid FTP response line: {line:?}"),
            },
        ));
    }

    let code = line[..3].parse::<u16>().map_err(|_| {
        Aria2Error::Recoverable(RecoverableError::FtpProtocolError {
            message: format!("Invalid FTP response code: {line:?}"),
        })
    })?;

    Ok((code, bytes[3] == b'-', &line[4..]))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::read_response_impl;
    use crate::error::{Aria2Error, RecoverableError};

    #[tokio::test]
    async fn preserves_multiline_response() {
        let response = b"211-Features:\r\n UTF8\r\n211 End\r\n";
        let mut reader = tokio::io::BufReader::new(std::io::Cursor::new(response));

        let (code, message) = read_response_impl(&mut reader, Duration::from_secs(1))
            .await
            .expect("multiline FTP response should parse");

        assert_eq!(code, 211);
        assert_eq!(message, "Features:\n UTF8\nEnd");
    }

    #[tokio::test]
    async fn rejects_oversized_response() {
        let response = format!("211-{}\r\n211 End\r\n", "x".repeat(65_536));
        let mut reader = tokio::io::BufReader::new(std::io::Cursor::new(response.into_bytes()));

        let error = read_response_impl(&mut reader, Duration::from_secs(1))
            .await
            .expect_err("oversized FTP response must be rejected");

        assert!(error.to_string().contains("Max FTP recv buffer reached"));
    }

    #[tokio::test]
    async fn rejects_eof_and_malformed_first_lines() {
        for response in [
            Vec::new(),
            b"220 Welcome".to_vec(),
            b"hello\r\n".to_vec(),
            b"220x Welcome\r\n".to_vec(),
        ] {
            let mut reader = tokio::io::BufReader::new(std::io::Cursor::new(response.clone()));
            let error = read_response_impl(&mut reader, Duration::from_secs(1))
                .await
                .expect_err("invalid FTP response must be rejected");

            if response.ends_with(b"\r\n") {
                assert!(matches!(
                    error,
                    Aria2Error::Recoverable(RecoverableError::FtpProtocolError { .. })
                ));
            } else {
                assert!(matches!(
                    error,
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. })
                ));
            }
        }
    }

    #[tokio::test]
    async fn rejects_incomplete_multiline_response() {
        let response = b"211-Features:\r\n UTF8\r\n";
        let mut reader = tokio::io::BufReader::new(std::io::Cursor::new(response));

        let error = read_response_impl(&mut reader, Duration::from_secs(1))
            .await
            .expect_err("incomplete multiline response must be rejected");

        assert!(matches!(
            error,
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. })
        ));
    }
}
