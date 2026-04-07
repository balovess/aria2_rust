use tracing::debug;

pub struct HttpHeaderProcessor;

impl HttpHeaderProcessor {
    pub fn extract_filename(content_disposition: &str) -> Option<String> {
        let cd = content_disposition.trim();

        let filename_star = Self::extract_filename_star(cd);
        if filename_star.is_some() {
            return filename_star;
        }

        Self::extract_filename_regular(cd)
    }

    fn extract_filename_star(cd: &str) -> Option<String> {
        for part in cd.split(';') {
            let part = part.trim();
            if let Some(rest) = part.strip_prefix("filename*=") {
                let rest = rest.trim().trim_matches('"');
                if let Some(encoded) = rest.split_once('\'') {
                    let (_charset, encoded_name) = encoded;
                    return Some(Self::decode_rfc5987(encoded_name));
                }
                return Some(rest.to_string());
            }
        }
        None
    }

    fn extract_filename_regular(cd: &str) -> Option<String> {
        let cd = cd.trim();
        let pos = cd.find("filename=")?;
        let rest = &cd[pos + 9..];
        let rest = rest.trim();
        if rest.starts_with('"') {
            if let Some(end_quote) = rest[1..].find('"') {
                return Some(rest[1..end_quote + 1].to_string());
            }
        } else {
            let end_pos = rest.find(';').unwrap_or(rest.len());
            return Some(rest[..end_pos].trim().to_string());
        }
        None
    }

    fn decode_rfc5987(input: &str) -> String {
        let mut bytes = Vec::with_capacity(input.len());
        let mut chars = input.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '%' {
                let hex: String = chars.by_ref().take(2).collect();
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    bytes.push(byte);
                } else {
                    bytes.extend(c.to_string().as_bytes());
                    bytes.extend(hex.as_bytes());
                }
            } else {
                bytes.push(c as u8);
            }
        }

        String::from_utf8_lossy(&bytes).to_string()
    }

    #[allow(dead_code)]
    fn decode_percent(input: &str) -> String {
        let mut result = String::with_capacity(input.len());
        let mut bytes = input.bytes().peekable();

        while let Some(b) = bytes.next() {
            if b == b'%' {
                let hex: Vec<u8> = bytes.by_ref().take(2).collect();
                if hex.len() == 2 {
                    let hex_str = unsafe { std::str::from_utf8_unchecked(&hex) };
                    if let Ok(byte) = u8::from_str_radix(hex_str, 16) {
                        result.push(byte as char);
                        continue;
                    }
                }
                result.push('%');
                for h in hex {
                    result.push(h as char);
                }
            } else {
                result.push(b as char);
            }
        }
        result
    }

    pub fn sanitize_filename(filename: &str) -> String {
        let forbidden = ['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
        let mut result = String::with_capacity(filename.len());
        for c in filename.chars() {
            if forbidden.contains(&c) {
                result.push('_');
            } else {
                result.push(c);
            }
        }
        if result.is_empty() || result == "." || result == ".." {
            "download".to_string()
        } else {
            result
        }
    }

    pub fn extract_extension(url: &str) -> Option<String> {
        let path = url.rsplit('/').next()?;
        let path = path.split('?').next()?.split('#').next()?;
        let dot_pos = path.rfind('.')?;
        let ext = &path[dot_pos + 1..];
        if ext.is_empty() {
            return None;
        }
        Some(ext.to_lowercase())
    }

    pub fn guess_filename_from_url(url: &str) -> Option<String> {
        let path = url.rsplit('/').next()?.split('?').next()?.split('#').next()?;
        if path.is_empty() || path == "/" {
            return None;
        }
        Some(path.to_string())
    }

    pub fn resolve_filename(
        url: &str,
        content_disposition: Option<&str>,
        default_name: &str,
    ) -> String {
        if let Some(cd) = content_disposition {
            if let Some(name) = Self::extract_filename(cd) {
                let sanitized = Self::sanitize_filename(&name);
                debug!("从Content-Disposition解析文件名: {}", sanitized);
                return sanitized;
            }
        }

        if let Some(name) = Self::guess_filename_from_url(url) {
            let sanitized = Self::sanitize_filename(&name);
            debug!("从URL路径解析文件名: {}", sanitized);
            return sanitized;
        }

        debug!("使用默认文件名: {}", default_name);
        default_name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_filename_regular() {
        let cd = r#"attachment; filename="example.zip""#;
        assert_eq!(
            HttpHeaderProcessor::extract_filename(cd),
            Some("example.zip".to_string())
        );
    }

    #[test]
    fn test_extract_filename_no_quotes() {
        let cd = "attachment; filename=example.zip";
        assert_eq!(
            HttpHeaderProcessor::extract_filename(cd),
            Some("example.zip".to_string())
        );
    }

    #[test]
    fn test_extract_filename_with_semicolon_in_name() {
        let cd = r#"attachment; filename="file;name.zip""#;
        assert_eq!(
            HttpHeaderProcessor::extract_filename(cd),
            Some("file;name.zip".to_string())
        );
    }

    #[test]
    fn test_extract_filename_star_rfc5987() {
        let cd = "attachment; filename*=UTF-8''%E4%B8%AD%E6%96%87%E6%96%87%E4%BB%B6.txt";
        let result = HttpHeaderProcessor::extract_filename(cd);
        assert!(result.is_some());
        assert!(result.unwrap().contains("中文"));
    }

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(
            HttpHeaderProcessor::sanitize_filename("file/name.zip"),
            "file_name.zip"
        );
        assert_eq!(HttpHeaderProcessor::sanitize_filename(".."), "download");
        assert_eq!(HttpHeaderProcessor::sanitize_filename("."), "download");
        assert_eq!(HttpHeaderProcessor::sanitize_filename(""), "download");
        assert_eq!(
            HttpHeaderProcessor::sanitize_filename("normal_file.tar.gz"),
            "normal_file.tar.gz"
        );
    }

    #[test]
    fn test_guess_filename_from_url() {
        assert_eq!(
            HttpHeaderProcessor::guess_filename_from_url("https://example.com/path/to/file.zip"),
            Some("file.zip".to_string())
        );
        assert_eq!(
            HttpHeaderProcessor::guess_filename_from_url("https://example.com/"),
            None
        );
    }

    #[test]
    fn test_resolve_filename_priority() {
        let resolved = HttpHeaderProcessor::resolve_filename(
            "https://example.com/download",
            Some(r#"attachment; filename="real_name.zip""#),
            "default.bin",
        );
        assert_eq!(resolved, "real_name.zip");

        let resolved_fallback = HttpHeaderProcessor::resolve_filename(
            "https://example.com/path/file.bin",
            None,
            "default.bin",
        );
        assert_eq!(resolved_fallback, "file.bin");
    }

    #[test]
    fn test_extract_extension() {
        assert_eq!(
            HttpHeaderProcessor::extract_extension("https://example.com/file.tar.gz?v=1"),
            Some("gz".to_string())
        );
        assert_eq!(
            HttpHeaderProcessor::extract_extension("https://example.com/nofile"),
            None
        );
    }
}
