use crate::error::{Aria2Error, Result};
use std::path::PathBuf;

const MAX_PATH_LEN: usize = 4096;

pub fn validate_dir_path(path: &str) -> Result<PathBuf> {
    if path.is_empty() {
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
            "Directory path must not be empty".into(),
        )));
    }
    if path.contains('\0') {
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
            "Path contains illegal characters".into(),
        )));
    }
    if path.len() > MAX_PATH_LEN {
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
            "Path too long".into(),
        )));
    }
    Ok(PathBuf::from(path))
}

static ILLEGAL_FILENAME_CHARS: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
static WINDOWS_RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

pub fn validate_out_filename(name: &str) -> Result<String> {
    if name.is_empty() {
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
            "Filename must not be empty".into(),
        )));
    }
    let stem = name.split('.').next().unwrap_or("");
    let upper = stem.to_uppercase();
    if WINDOWS_RESERVED_NAMES.contains(&upper.as_str()) {
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
            format!("Reserved name: {}", stem),
        )));
    }
    for c in ILLEGAL_FILENAME_CHARS {
        if name.contains(*c) {
            return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                format!("Illegal character: {}", c),
            )));
        }
    }
    Ok(name.to_string())
}

pub fn validate_split_value(n: u16) -> Result<u16> {
    if n == 0 || n > 16 {
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
            "split value must be between 1 and 16".into(),
        )));
    }
    Ok(n)
}

pub fn validate_connection_limit(n: u16) -> Result<u16> {
    if n == 0 || n > 16 {
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
            "connection count must be between 1 and 16".into(),
        )));
    }
    Ok(n)
}
