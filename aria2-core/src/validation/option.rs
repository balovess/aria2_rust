use std::path::PathBuf;
use crate::error::{Aria2Error, Result};

const MAX_PATH_LEN: usize = 4096;

pub fn validate_dir_path(path: &str) -> Result<PathBuf> {
    if path.is_empty() {
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config("目录路径不能为空".into())));
    }
    if path.contains('\0') {
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config("路径包含非法字符".into())));
    }
    if path.len() > MAX_PATH_LEN {
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config("路径过长".into())));
    }
    Ok(PathBuf::from(path))
}

static ILLEGAL_FILENAME_CHARS: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
static WINDOWS_RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL",
    "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
    "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

pub fn validate_out_filename(name: &str) -> Result<String> {
    if name.is_empty() {
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config("文件名不能为空".into())));
    }
    let stem = name.split('.').next().unwrap_or("");
    let upper = stem.to_uppercase();
    if WINDOWS_RESERVED_NAMES.contains(&upper.as_str()) {
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config(format!("保留名称: {}", stem).into())));
    }
    for c in ILLEGAL_FILENAME_CHARS {
        if name.contains(*c) {
            return Err(Aria2Error::Fatal(crate::error::FatalError::Config(format!("非法字符: {}", c).into())));
        }
    }
    Ok(name.to_string())
}

pub fn validate_split_value(n: u16) -> Result<u16> {
    if n == 0 || n > 16 {
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config("split值必须在1-16之间".into())));
    }
    Ok(n)
}

pub fn validate_connection_limit(n: u16) -> Result<u16> {
    if n == 0 || n > 16 {
        return Err(Aria2Error::Fatal(crate::error::FatalError::Config("连接数必须在1-16之间".into())));
    }
    Ok(n)
}
