use crate::error::{Aria2Error, Result};

const SUPPORTED_SCHEMES: &[&str] = &["http", "https", "ftp", "sftp", "file"];
const DANGEROUS_SCHEMES: &[&str] = &["javascript", "data", "vbscript"];

const URI_MAX_FILENAME_LEN: usize = 255;

#[derive(Debug, Clone)]
pub struct ValidatedUri {
    pub original: String,
    pub scheme: String,
    pub is_magnet: bool,
    pub is_torrent: bool,
}

pub fn validate(uri: &str) -> Result<ValidatedUri> {
    let trimmed = uri.trim();
    if trimmed.is_empty() {
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config("URI不能为空".into())));
    }

    if trimmed.starts_with("magnet:?") || trimmed.starts_with("magnet?") {
        return Ok(ValidatedUri {
            original: trimmed.to_string(),
            scheme: "magnet".to_string(),
            is_magnet: true,
            is_torrent: false,
        });
    }

    let (scheme, rest) = match trimmed.split_once("://") {
        Some(pair) => pair,
        None => return Err(Aria2Error::Fatal(crate::error::FatalError::Config("URI缺少协议前缀".into()))),
    };

    let lower_scheme = scheme.to_lowercase();
    for dangerous in DANGEROUS_SCHEMES {
        if lower_scheme == *dangerous {
            return Err(Aria2Error::Fatal(crate::error::FatalError::Config(format!("不安全的协议: {}", scheme).into())));
        }
    }
    if !SUPPORTED_SCHEMES.contains(&lower_scheme.as_str()) && lower_scheme != "magnet" {
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config(format!("不支持的协议: {}", scheme).into())));
    }
    if rest.is_empty() {
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config("URI缺少路径".into())));
    }

    Ok(ValidatedUri {
        original: trimmed.to_string(),
        scheme: lower_scheme.clone(),
        is_magnet: false,
        is_torrent: lower_scheme == "file" && rest.ends_with(".torrent"),
    })
}

pub fn is_magnet_link(uri: &str) -> bool {
    let t = uri.trim().to_lowercase();
    t.starts_with("magnet:?") || t.starts_with("magnet?")
}

pub fn is_torrent_file(path: &str) -> bool {
    path.trim().ends_with(".torrent")
}

pub fn sanitize_filename_from_uri(uri: &str) -> String {
    let uri = uri.trim();
    let path_part = uri
        .rsplit('/')
        .next()
        .unwrap_or("")
        .rsplit('\\')
        .next()
        .unwrap_or("");

    let decoded = urlencoding_decode(path_part);
    let cleaned = remove_traversal(&decoded);
    truncate_filename(&cleaned)
}

fn urlencoding_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if hex.len() == 2 {
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    result.push(byte as char);
                    continue;
                }
            }
            result.push(c);
        } else {
            result.push(c);
        }
    }
    result
}

fn remove_traversal(s: &str) -> String {
    s.replace("../", "").replace("..\\", "").replace("./", "")
}

fn truncate_filename(s: &str) -> String {
    if s.len() > URI_MAX_FILENAME_LEN {
        s[..URI_MAX_FILENAME_LEN].to_string()
    } else {
        s.to_string()
    }
}
