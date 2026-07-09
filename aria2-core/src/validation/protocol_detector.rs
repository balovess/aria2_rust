use crate::error::{Aria2Error, FatalError, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum InputType {
    HttpUrl,
    FtpUrl,
    SftpUrl,
    TorrentFile,
    MetalinkFile,
    MagnetLink,
}

impl std::fmt::Display for InputType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HttpUrl => write!(f, "http/https"),
            Self::FtpUrl => write!(f, "ftp"),
            Self::SftpUrl => write!(f, "sftp"),
            Self::TorrentFile => write!(f, "torrent"),
            Self::MetalinkFile => write!(f, "metalink"),
            Self::MagnetLink => write!(f, "magnet"),
        }
    }
}

pub struct DetectedInput {
    pub input_type: InputType,
    pub raw: String,
    pub file_data: Option<Vec<u8>>,
}

fn looks_like_torrent(data: &[u8]) -> bool {
    data.len() >= 3 && data[0] == b'd' && data[1] == b'8' && data[2] == b':'
}

fn looks_like_metalink(data: &[u8]) -> bool {
    let preview = &data[..data.len().min(200)];
    let start = String::from_utf8_lossy(preview);
    let lower = start.to_lowercase();
    lower.contains("<metalink") || lower.contains("xmlns=\"urn:ietf:params:xml:ns:metalink")
}

pub fn detect(input: &str) -> Result<DetectedInput> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(Aria2Error::Fatal(FatalError::Config(
            "Input is empty".into(),
        )));
    }

    let lower = trimmed.to_lowercase();

    if lower.starts_with("magnet:?") || lower.starts_with("magnet?") {
        return Ok(DetectedInput {
            input_type: InputType::MagnetLink,
            raw: trimmed.to_string(),
            file_data: None,
        });
    }

    if let Some((scheme, _)) = trimmed.split_once("://") {
        match scheme.to_lowercase().as_str() {
            "http" | "https" => {
                return Ok(DetectedInput {
                    input_type: InputType::HttpUrl,
                    raw: trimmed.to_string(),
                    file_data: None,
                });
            }
            "ftp" => {
                return Ok(DetectedInput {
                    input_type: InputType::FtpUrl,
                    raw: trimmed.to_string(),
                    file_data: None,
                });
            }
            "sftp" => {
                return Ok(DetectedInput {
                    input_type: InputType::SftpUrl,
                    raw: trimmed.to_string(),
                    file_data: None,
                });
            }
            "file" => {}
            _ => {}
        }
    }

    let path = std::path::Path::new(trimmed);
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(trimmed)
        .to_lowercase();

    if filename.ends_with(".torrent") {
        let data = std::fs::read(path).map_err(|e| {
            Aria2Error::Fatal(FatalError::Config(format!(
                "Cannot read torrent file: {}",
                e
            )))
        })?;
        if !looks_like_torrent(&data) {
            return Err(Aria2Error::Fatal(FatalError::Config(
                "File does not look like a valid .torrent (missing BEncode header)".into(),
            )));
        }
        return Ok(DetectedInput {
            input_type: InputType::TorrentFile,
            raw: trimmed.to_string(),
            file_data: Some(data),
        });
    }

    if filename.ends_with(".metalink")
        || filename.ends_with(".meta4")
        || filename.ends_with(".meta4")
    {
        let data = std::fs::read(path).map_err(|e| {
            Aria2Error::Fatal(FatalError::Config(format!(
                "Cannot read metalink file: {}",
                e
            )))
        })?;
        if !looks_like_metalink(&data) {
            return Err(Aria2Error::Fatal(FatalError::Config(
                "File does not look like a valid metalink XML".into(),
            )));
        }
        return Ok(DetectedInput {
            input_type: InputType::MetalinkFile,
            raw: trimmed.to_string(),
            file_data: Some(data),
        });
    }

    if path.exists() {
        let data = std::fs::read(path).map_err(|e| {
            Aria2Error::Fatal(FatalError::Config(format!("Cannot read file: {}", e)))
        })?;
        if looks_like_torrent(&data) {
            return Ok(DetectedInput {
                input_type: InputType::TorrentFile,
                raw: trimmed.to_string(),
                file_data: Some(data),
            });
        }
        if looks_like_metalink(&data) {
            return Ok(DetectedInput {
                input_type: InputType::MetalinkFile,
                raw: trimmed.to_string(),
                file_data: Some(data),
            });
        }
        // A local file that exists but is neither a torrent nor a metalink must
        // not be reinterpreted as an HTTP URL. Returning an error here prevents
        // the app from downloading its own executable (or any other local file)
        // when a path accidentally leaks into detect().
        return Err(Aria2Error::Fatal(FatalError::Config(format!(
            "File exists but is not a .torrent or .metalink: {}",
            trimmed
        ))));
    }

    // Original aria2c never guesses scheme-less inputs as URLs.
    // Reject ambiguous inputs and require users to provide full URIs.
    Err(Aria2Error::Fatal(FatalError::Config(format!(
        "Cannot detect input type for: {}. Please provide:\n  - A full URL with scheme (http://, https://, ftp://, sftp://)\n  - A magnet link (magnet:?xt=...)\n  - A .torrent or .metalink file path\n  - Or use --input-file to load URIs from a file",
        trimmed
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_http_url() {
        let d = detect("http://example.com/file.zip").unwrap();
        assert_eq!(d.input_type, InputType::HttpUrl);
        assert_eq!(d.raw, "http://example.com/file.zip");
        assert!(d.file_data.is_none());
    }

    #[test]
    fn test_detect_https_url() {
        let d = detect("https://example.com/file.iso").unwrap();
        assert_eq!(d.input_type, InputType::HttpUrl);
    }

    #[test]
    fn test_detect_ftp_url() {
        let d = detect("ftp://server/path/file.bin").unwrap();
        assert_eq!(d.input_type, InputType::FtpUrl);
    }

    #[test]
    fn test_detect_sftp_url() {
        let d = detect("sftp://user@host/path").unwrap();
        assert_eq!(d.input_type, InputType::SftpUrl);
    }

    #[test]
    fn test_detect_magnet_link() {
        let d = detect("magnet:?xt=urn:btih:abc123&dn=test").unwrap();
        assert_eq!(d.input_type, InputType::MagnetLink);
    }

    #[test]
    fn test_detect_empty_input() {
        assert!(detect("").is_err());
        assert!(detect("   ").is_err());
    }

    #[test]
    fn test_detect_existing_local_non_container_file_is_rejected() {
        // Regression guard: an existing local file that is neither a torrent nor
        // a metalink must NOT be turned into an http:// URL. This previously
        // caused the app to download its own executable on startup (the path
        // contained a separator and matched the bare-hostname fallback).
        let dir = tempfile::tempdir().expect("create temp dir");
        let file_path = dir.path().join("aria2c.exe");
        std::fs::write(&file_path, b"this is a plain binary, not a torrent")
            .expect("write file");

        let detected = detect(file_path.to_str().unwrap());
        assert!(
            detected.is_err(),
            "Existing local non-torrent/non-metalink file must be rejected, but detect() succeeded"
        );
    }

    #[test]
    fn test_detect_bare_hostname_is_rejected() {
        // Bare hostname/path without scheme must be rejected.
        // This aligns with original aria2c behavior which never guesses scheme-less inputs.
        let result = detect("example.com/path/file.zip");
        assert!(
            result.is_err(),
            "Bare hostname without scheme must be rejected, but detect() succeeded"
        );
        let err_msg = match result {
            Err(e) => e.to_string(),
            Ok(_) => unreachable!("Already asserted is_err()"),
        };
        assert!(
            err_msg.contains("Cannot detect input type"),
            "Error message should mention detection failure"
        );
        assert!(
            err_msg.contains("full URL with scheme"),
            "Error message should suggest using a full URL with scheme"
        );
    }

    #[test]
    fn test_input_type_display() {
        assert_eq!(InputType::HttpUrl.to_string(), "http/https");
        assert_eq!(InputType::FtpUrl.to_string(), "ftp");
        assert_eq!(InputType::TorrentFile.to_string(), "torrent");
        assert_eq!(InputType::MetalinkFile.to_string(), "metalink");
        assert_eq!(InputType::MagnetLink.to_string(), "magnet");
    }
}
