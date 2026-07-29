//! Helper types and pure functions for FTP download command.

use crate::constants;
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};

/// URL-encoded string decoder.
pub(crate) fn urlencoding_decode(s: &str) -> String {
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

/// Parse FTP URI into `(host, port, username, password, remote_path)`.
pub(crate) fn parse_uri(uri: &str) -> Result<(String, u16, String, String, String)> {
    if !uri.starts_with("ftp://") && !uri.starts_with("ftps://") {
        return Err(Aria2Error::Fatal(FatalError::UnsupportedProtocol {
            protocol: "ftp".into(),
        }));
    }

    let without_scheme = uri
        .trim_start_matches("ftp://")
        .trim_start_matches("ftps://");

    let (auth_host_port, path) = match without_scheme.find('/') {
        Some(idx) => (&without_scheme[..idx], &without_scheme[idx..]),
        None => (without_scheme, "/"),
    };

    let (auth, host_port) = match auth_host_port.rfind('@') {
        Some(idx) => (&auth_host_port[..idx], &auth_host_port[idx + 1..]),
        None => ("", auth_host_port),
    };

    let (username, password) = if auth.is_empty() {
        (
            constants::FTP_DEFAULT_USER.to_string(),
            constants::FTP_DEFAULT_PASSWORD.to_string(),
        )
    } else if let Some(colon_pos) = auth.find(':') {
        (
            auth[..colon_pos].to_string(),
            auth[colon_pos + 1..].to_string(),
        )
    } else {
        (auth.to_string(), String::new())
    };

    let (host, port) = match host_port.rfind(':') {
        Some(idx) => (
            host_port[..idx].to_string(),
            host_port[idx + 1..]
                .parse::<u16>()
                .unwrap_or(constants::FTP_DEFAULT_PORT),
        ),
        None => (host_port.to_string(), constants::FTP_DEFAULT_PORT),
    };

    Ok((host, port, username, password, urlencoding_decode(path)))
}

/// Extract the filename portion from a remote path.
pub(crate) fn extract_filename(remote_path: &str) -> Option<String> {
    remote_path
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty() && *s != "/")
        .map(|s| s.to_string())
}

/// Classify FTP response code to determine error handling strategy.
#[allow(dead_code)] // Must remain: will be used when FTP retry-with-classification logic is integrated
pub(crate) fn classify_ftp_error(
    code: u16,
    message: &str,
    host: &str,
    port: u16,
    remote_path: &str,
) -> Aria2Error {
    match code {
        // Positive responses (should not be errors)
        100..=399 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Unexpected positive response: {} {}", code, message),
        }),
        // Transient negative completion - retry may succeed
        421 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Service not available: {}", message),
        }),
        425 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Can't open data connection: {}", message),
        }),
        426 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Connection closed; transfer aborted: {}", message),
        }),
        450 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Requested file action not taken: {}", message),
        }),
        451 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Requested action aborted: {}", message),
        }),
        452 => Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Requested action not taken: {}", message),
        }),
        // Permanent negative completion - do not retry
        500..=504 => Aria2Error::Fatal(FatalError::Config(format!(
            "FTP syntax error: {} {}",
            code, message
        ))),
        530 => Aria2Error::Fatal(FatalError::PermissionDenied {
            path: format!("{}:{}", host, port),
        }),
        532 => Aria2Error::Fatal(FatalError::PermissionDenied {
            path: "Account required for storing file".into(),
        }),
        550 => Aria2Error::Fatal(FatalError::FileNotFound {
            path: remote_path.to_string(),
        }),
        551 => Aria2Error::Fatal(FatalError::Config(format!(
            "Page type unknown: {}",
            message
        ))),
        552 => Aria2Error::Fatal(FatalError::Config(format!(
            "Exceeded storage allocation: {}",
            message
        ))),
        553 => Aria2Error::Fatal(FatalError::PermissionDenied {
            path: format!("Filename not allowed: {}", message),
        }),
        // Unknown error codes
        _ => {
            // Check message content for hints about error type
            let msg_lower = message.to_lowercase();
            if msg_lower.contains("not found")
                || msg_lower.contains("no such")
                || msg_lower.contains("access denied")
                || msg_lower.contains("permission")
            {
                Aria2Error::Fatal(FatalError::FileNotFound {
                    path: remote_path.to_string(),
                })
            } else if msg_lower.contains("login") || msg_lower.contains("auth") {
                Aria2Error::Fatal(FatalError::PermissionDenied {
                    path: format!("{}:{}", host, port),
                })
            } else {
                // Default to recoverable for unknown codes in 4xx/5xx range
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("FTP error {} {}: {}", code, message, remote_path),
                })
            }
        }
    }
}
