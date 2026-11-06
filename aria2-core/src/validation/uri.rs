use crate::error::{Aria2Error, Result};
use url::Url;

const SUPPORTED_SCHEMES: &[&str] = &["http", "https", "ftp", "sftp", "file"];
const DANGEROUS_SCHEMES: &[&str] = &["javascript", "data", "vbscript"];

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
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
            "URI must not be empty".into(),
        )));
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
        None => {
            return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                "URI missing scheme prefix".into(),
            )));
        }
    };

    let lower_scheme = scheme.to_lowercase();
    for dangerous in DANGEROUS_SCHEMES {
        if lower_scheme == *dangerous {
            return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                format!("Insecure scheme: {}", scheme),
            )));
        }
    }
    if !SUPPORTED_SCHEMES.contains(&lower_scheme.as_str()) && lower_scheme != "magnet" {
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
            format!("Unsupported scheme: {}", scheme),
        )));
    }
    if rest.is_empty() {
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
            "URI missing path component".into(),
        )));
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
    let Some(raw_segment) = raw_uri_path_segment(uri) else {
        return DEFAULT_FILENAME.to_owned();
    };

    let decoded = crate::util::uri::percent_decode(&raw_segment);
    sanitize_filename_candidate(&decoded).unwrap_or_else(|| DEFAULT_FILENAME.to_owned())
}

pub(crate) fn sanitize_filename_candidate(candidate: &str) -> Option<String> {
    let basename = candidate
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())?;

    let mut name: String = basename
        .chars()
        .filter(|ch| !ch.is_control())
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            _ => ch,
        })
        .collect();

    name = name.trim_end_matches([' ', '.']).to_owned();
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }

    if is_windows_reserved_name(&name) {
        name.insert(0, '_');
    }

    if name.len() > MAX_FILENAME_BYTES {
        let mut end = MAX_FILENAME_BYTES;
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
        name = name.trim_end_matches([' ', '.']).to_owned();
    }

    (!name.is_empty()).then_some(name)
}

const DEFAULT_FILENAME: &str = "index.html";
const MAX_FILENAME_BYTES: usize = 255;

fn raw_uri_path_segment(uri: &str) -> Option<String> {
    if let Ok(parsed) = Url::parse(uri) {
        if parsed.path().ends_with('/') {
            return None;
        }
        return parsed
            .path()
            .rsplit('/')
            .find(|segment| !segment.is_empty())
            .map(str::to_owned);
    }

    let without_suffix = uri.split(['?', '#']).next().unwrap_or_default();
    let path = if let Some(scheme_end) = without_suffix.find("://") {
        let authority_and_path = &without_suffix[scheme_end + 3..];
        let slash = authority_and_path.find('/')?;
        &authority_and_path[slash..]
    } else {
        without_suffix
    };

    if path.ends_with('/') {
        return None;
    }

    path.rsplit('/')
        .find(|segment| !segment.is_empty())
        .map(str::to_owned)
}

fn is_windows_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or_default();
    let bytes = stem.as_bytes();
    stem.eq_ignore_ascii_case("CON")
        || stem.eq_ignore_ascii_case("PRN")
        || stem.eq_ignore_ascii_case("AUX")
        || stem.eq_ignore_ascii_case("NUL")
        || (bytes.len() == 4
            && (bytes[..3].eq_ignore_ascii_case(b"COM") || bytes[..3].eq_ignore_ascii_case(b"LPT"))
            && bytes[3].is_ascii_digit()
            && bytes[3] != b'0')
}
