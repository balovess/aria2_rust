use tracing::debug;

/// HTTP status codes that aria2 treats as redirects.
///
/// `304 Not Modified` is intentionally excluded: it is a conditional
/// response, not a redirect, even though it is in the 3xx range.
pub fn is_redirect_status(status_code: u16) -> bool {
    matches!(status_code, 300..=303 | 307 | 308)
}

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
        // Match C++ `HttpResponse::isRedirect()`: a recognized redirect status
        // is actionable only when the response supplies a Location header.
        is_redirect_status(self.status_code) && self.location().is_some()
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
        let range_part = value
            .strip_prefix("bytes")
            .map(|rest| rest.trim_start_matches([' ', '\t', '=']))
            .unwrap_or(value);
        let parts: Vec<&str> = range_part.split('/').collect();
        if parts.len() != 2 {
            debug!("Content-Range format error: split failed");
            return None;
        }

        let range_values: Vec<&str> = parts[0].split('-').collect();
        if range_values.len() != 2 {
            debug!("Content-Range range parse failed");
            return None;
        }

        let start: u64 = range_values[0].trim().parse().ok()?;
        let end: u64 = range_values[1].trim().parse().ok()?;
        let total_size = match parts[1].trim() {
            "*" => None,
            s => s.parse().ok(),
        };
        if start > end || total_size.is_some_and(|total| end >= total) {
            debug!(start, end, ?total_size, "Content-Range bounds are invalid");
            return None;
        }

        Some(Self {
            unit: "bytes".to_string(),
            start,
            end,
            total_size,
        })
    }

    pub fn size(&self) -> u64 {
        self.end
            .checked_sub(self.start)
            .map_or(0, |difference| difference.saturating_add(1))
    }
}

/// Parse an unsatisfied byte range such as `bytes */1234` and return its
/// complete representation length.
pub fn parse_unsatisfied_content_range_length(value: &str) -> Option<u64> {
    value.trim().strip_prefix("bytes */")?.trim().parse().ok()
}

/// Parse the HTTP `Retry-After` value as delay-seconds or an HTTP date.
///
/// `now_epoch_secs` is supplied by the caller so the protocol parser remains
/// deterministic; retry caps and scheduling policy stay with the caller.
pub fn parse_retry_after(value: &str, now_epoch_secs: i64) -> Option<std::time::Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(std::time::Duration::from_secs(seconds));
    }
    let retry_at = crate::http::cookie::parsing::parse_http_date(value)?;
    Some(std::time::Duration::from_secs(
        u64::try_from(retry_at.saturating_sub(now_epoch_secs)).unwrap_or(0),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::header_processor::HttpResponseHead;
    use crate::http::request::HttpRequest;

    #[test]
    fn parses_retry_after_seconds_and_http_dates() {
        assert_eq!(
            parse_retry_after(" 42 ", 0),
            Some(std::time::Duration::from_secs(42))
        );
        assert_eq!(parse_retry_after("not a date", 0), None);
        assert_eq!(
            parse_retry_after("Wed, 09 Jun 2021 10:18:14 GMT", 1_623_233_900),
            Some(std::time::Duration::ZERO)
        );
        assert_eq!(
            parse_retry_after("Wed, 09 Jun 2021 10:18:14 GMT", 1_623_233_000),
            Some(std::time::Duration::from_secs(894))
        );
    }

    #[test]
    fn test_response_status_checks() {
        let resp_ok = HttpResponse::new(200, "OK".into());
        assert!(resp_ok.is_success());
        assert!(!resp_ok.is_redirect());
        assert!(!resp_ok.is_partial_content());

        let resp_206 = HttpResponse::new(206, "Partial Content".into());
        assert!(resp_206.is_success());
        assert!(resp_206.is_partial_content());

        let mut resp_301 = HttpResponse::new(301, "Moved".into());
        resp_301
            .headers
            .push(("Location".to_string(), "/new".to_string()));
        assert!(resp_301.is_redirect());

        let resp_302_without_location = HttpResponse::new(302, "Found".into());
        assert!(!resp_302_without_location.is_redirect());

        let mut resp_300 = HttpResponse::new(300, "Multiple Choices".into());
        resp_300
            .headers
            .push(("Location".to_string(), "/choice".to_string()));
        assert!(resp_300.is_redirect());

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
    fn response_head_from_parts_normalizes_header_names() {
        let head = HttpResponseHead::from_parts(
            "HTTP/1.1".to_string(),
            200,
            "OK".to_string(),
            vec![("Content-Type".to_string(), "text/plain".to_string())],
        );

        assert_eq!(
            head.iter_headers().next(),
            Some(("content-type", "text/plain"))
        );
        assert_eq!(head.header("CONTENT-TYPE"), Some("text/plain"));
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
        assert_eq!(
            ContentRange::parse("bytes=0-499/1000").map(|range| (range.start, range.end)),
            Some((0, 499))
        );
        assert_eq!(
            ContentRange::parse("0-499/1000").map(|range| (range.start, range.end)),
            Some((0, 499))
        );
        assert!(ContentRange::parse("bytes 10-9/1000").is_none());
        assert!(ContentRange::parse("bytes 0-1000/1000").is_none());
        assert_eq!(ContentRange::parse("bytes 0-499/*").unwrap().size(), 500);
        assert_eq!(
            ContentRange {
                unit: "bytes".to_string(),
                start: 1,
                end: 0,
                total_size: None,
            }
            .size(),
            0
        );
    }

    #[test]
    fn parses_unsatisfied_content_range_length() {
        assert_eq!(
            parse_unsatisfied_content_range_length("bytes */1234"),
            Some(1234)
        );
        assert_eq!(parse_unsatisfied_content_range_length("bytes */*"), None);
        assert_eq!(parse_unsatisfied_content_range_length("bytes 0-1/2"), None);
    }

    #[test]
    fn test_request_builder() {
        let req = HttpRequest::get("https://example.com/file.bin")
            .with_range(500, Some(999))
            .with_user_agent(crate::identity::DEFAULT_USER_AGENT);

        assert_eq!(req.method, "GET");
        assert!(req.has_range());
        assert_eq!(req.get_header("Range"), Some(&"bytes=500-999".to_string()));
        assert_eq!(
            req.get_header("User-Agent"),
            Some(&crate::identity::DEFAULT_USER_AGENT.to_string())
        );
    }
}
