use tracing::debug;

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status_code: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn new(status_code: u16, status_text: String) -> Self {
        Self {
            status_code,
            status_text,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status_code)
    }

    pub fn is_redirect(&self) -> bool {
        [301, 302, 303, 307, 308].contains(&self.status_code)
    }

    pub fn is_partial_content(&self) -> bool {
        self.status_code == 206
    }

    pub fn is_client_error(&self) -> bool {
        (400..500).contains(&self.status_code)
    }

    pub fn is_server_error(&self) -> bool {
        (500..600).contains(&self.status_code)
    }

    pub fn header(&self, name: &str) -> Option<&String> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    }

    pub fn header_all(&self, name: &str) -> Vec<&String> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
            .collect()
    }

    pub fn content_length(&self) -> Option<u64> {
        self.header("content-length")
            .and_then(|v| v.parse::<u64>().ok())
    }

    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type").map(|s| s.as_str())
    }

    pub fn content_encoding(&self) -> Option<&str> {
        self.header("content-encoding").map(|s| s.as_str())
    }

    pub fn content_disposition(&self) -> Option<&str> {
        self.header("content-disposition").map(|s| s.as_str())
    }

    pub fn accept_ranges(&self) -> bool {
        self.header("accept-ranges")
            .map(|v| v.eq_ignore_ascii_case("bytes"))
            .unwrap_or(false)
    }

    pub fn location(&self) -> Option<&str> {
        self.header("location").map(|s| s.as_str())
    }

    pub fn etag(&self) -> Option<&str> {
        self.header("etag").map(|s| s.as_str())
    }

    pub fn last_modified(&self) -> Option<&str> {
        self.header("last-modified").map(|s| s.as_str())
    }

    pub fn server(&self) -> Option<&str> {
        self.header("server").map(|s| s.as_str())
    }

    pub fn connection(&self) -> Option<&str> {
        self.header("connection").map(|s| s.as_str())
    }

    pub fn transfer_encoding(&self) -> Option<&str> {
        self.header("transfer-encoding").map(|s| s.as_str())
    }

    pub fn body_as_utf8(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.body)
    }

    pub fn body_len(&self) -> usize {
        self.body.len()
    }

    pub fn parse_content_range(&self) -> Option<ContentRange> {
        let raw = self.header("content-range")?;
        ContentRange::parse(raw)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentRange {
    pub unit: String,
    pub start: u64,
    pub end: u64,
    pub total_size: Option<u64>,
}

impl ContentRange {
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if !value.starts_with("bytes ") {
            debug!("Content-Range格式异常: 非bytes单位");
            return None;
        }

        let range_part = &value[6..];
        let parts: Vec<&str> = range_part.split('/').collect();
        if parts.len() != 2 {
            debug!("Content-Range格式异常: 分割失败");
            return None;
        }

        let range_values: Vec<&str> = parts[0].split('-').collect();
        if range_values.len() != 2 {
            debug!("Content-Range范围解析失败");
            return None;
        }

        let start: u64 = range_values[0].trim().parse().ok()?;
        let end: u64 = range_values[1].trim().parse().ok()?;
        let total_size = match parts[1].trim() {
            "*" => None,
            s => s.parse().ok(),
        };

        Some(Self {
            unit: "bytes".to_string(),
            start,
            end,
            total_size,
        })
    }

    pub fn size(&self) -> u64 {
        self.end - self.start + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::request::HttpRequest;

    #[test]
    fn test_response_status_checks() {
        let resp_ok = HttpResponse::new(200, "OK".into());
        assert!(resp_ok.is_success());
        assert!(!resp_ok.is_redirect());
        assert!(!resp_ok.is_partial_content());

        let resp_206 = HttpResponse::new(206, "Partial Content".into());
        assert!(resp_206.is_success());
        assert!(resp_206.is_partial_content());

        let resp_301 = HttpResponse::new(301, "Moved".into());
        assert!(resp_301.is_redirect());

        let resp_404 = HttpResponse::new(404, "Not Found".into());
        assert!(resp_404.is_client_error());

        let resp_500 = HttpResponse::new(500, "Server Error".into());
        assert!(resp_500.is_server_error());
    }

    #[test]
    fn test_headers_lookup() {
        let mut resp = HttpResponse::new(200, "OK".into());
        resp.headers
            .push(("Content-Length".to_string(), "1024".to_string()));
        resp.headers.push((
            "Content-Type".to_string(),
            "application/octet-stream".to_string(),
        ));

        assert_eq!(resp.content_length(), Some(1024));
        assert_eq!(resp.content_type(), Some("application/octet-stream"));
        assert_eq!(resp.header("content-length"), Some(&"1024".to_string()));
    }

    #[test]
    fn test_content_range_parse() {
        let cr = ContentRange::parse("bytes 0-499/1000").unwrap();
        assert_eq!(cr.start, 0);
        assert_eq!(cr.end, 499);
        assert_eq!(cr.total_size, Some(1000));
        assert_eq!(cr.size(), 500);

        let cr_unknown_total = ContentRange::parse("bytes 0-499/*").unwrap();
        assert_eq!(cr_unknown_total.total_size, None);

        assert!(ContentRange::parse("invalid").is_none());
        assert!(ContentRange::parse("bits 0-499/1000").is_none());
    }

    #[test]
    fn test_request_builder() {
        let req = HttpRequest::get("https://example.com/file.bin")
            .with_range(500, Some(999))
            .with_user_agent("aria2/1.37.0-Rust");

        assert_eq!(req.method, "GET");
        assert!(req.has_range());
        assert_eq!(req.get_header("Range"), Some(&"bytes=500-999".to_string()));
        assert_eq!(
            req.get_header("User-Agent"),
            Some(&"aria2/1.37.0-Rust".to_string())
        );
    }
}
