use std::path::{Path, PathBuf};

const ILLEGAL_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
const CONTROL_RANGE: std::ops::RangeInclusive<char> = '\x00'..='\x1f';

pub fn sanitize(raw: &str) -> String {
    if raw.is_empty() {
        return "download".to_string();
    }
    let mut result = String::with_capacity(raw.len());
    for c in raw.chars() {
        if CONTROL_RANGE.contains(&c) || ILLEGAL_CHARS.contains(&c) {
            result.push('_');
        } else {
            result.push(c);
        }
    }
    let trimmed = result.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if trimmed.is_empty() { "download".to_string() } else { trimmed.to_string() }
}

pub fn make_unique(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let path = std::path::Path::new(name);
    let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    for i in 1u32..=9999 {
        let new_name = format!("{}_{}{}", stem, i, ext);
        let path = dir.join(&new_name);
        if !path.exists() {
            return path;
        }
    }
    candidate
}
