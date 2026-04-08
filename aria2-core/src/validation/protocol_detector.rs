use crate::error::{Aria2Error, Result, FatalError};

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
        return Err(Aria2Error::Fatal(FatalError::Config("Input is empty".into())));
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
    let filename = path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(trimmed)
        .to_lowercase();

    if filename.ends_with(".torrent") {
        let data = std::fs::read(path).map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("Cannot read torrent file: {}", e))))?;
        if !looks_like_torrent(&data) {
            return Err(Aria2Error::Fatal(FatalError::Config("File does not look like a valid .torrent (missing BEncode header)".into())));
        }
        return Ok(DetectedInput {
            input_type: InputType::TorrentFile,
            raw: trimmed.to_string(),
            file_data: Some(data),
        });
    }

    if filename.ends_with(".metalink") || filename.ends_with(".meta4") || filename.ends_with(".meta4") {
        let data = std::fs::read(path).map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("Cannot read metalink file: {}", e))))?;
        if !looks_like_metalink(&data) {
            return Err(Aria2Error::Fatal(FatalError::Config("File does not look like a valid metalink XML".into())));
        }
        return Ok(DetectedInput {
            input_type: InputType::MetalinkFile,
            raw: trimmed.to_string(),
            file_data: Some(data),
        });
    }

    if path.exists() {
        let data = std::fs::read(path).map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("Cannot read file: {}", e))))?;
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
    }

    if trimmed.contains('/') || trimmed.contains('\\') {
        return Ok(DetectedInput {
            input_type: InputType::HttpUrl,
            raw: format!("http://{}", trimmed),
            file_data: None,
        });
    }

    Err(Aria2Error::Fatal(FatalError::Config(format!("Cannot detect input type for: {}", trimmed).into())))
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
    fn test_input_type_display() {
        assert_eq!(InputType::HttpUrl.to_string(), "http/https");
        assert_eq!(InputType::FtpUrl.to_string(), "ftp");
        assert_eq!(InputType::TorrentFile.to_string(), "torrent");
        assert_eq!(InputType::MetalinkFile.to_string(), "metalink");
        assert_eq!(InputType::MagnetLink.to_string(), "magnet");
    }
}
