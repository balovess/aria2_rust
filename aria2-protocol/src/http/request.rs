#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Option<Vec<(String, String)>>,
    pub body: Option<Vec<u8>>,
}

impl HttpRequest {
    pub fn new<U: Into<String>>(method: &str, url: U) -> Self {
        Self {
            method: method.to_string(),
            url: url.into(),
            headers: None,
            body: None,
        }
    }

    pub fn get<U: Into<String>>(url: U) -> Self {
        Self::new("GET", url)
    }

    pub fn post<U: Into<String>>(url: U) -> Self {
        Self::new("POST", url)
    }

    pub fn head<U: Into<String>>(url: U) -> Self {
        Self::new("HEAD", url)
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers
            .get_or_insert_with(Vec::new)
            .push((name.to_string(), value.to_string()));
        self
    }

    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = Some(headers);
        self
    }

    pub fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = Some(body);
        self
    }

    pub fn with_body_str(mut self, body: &str) -> Self {
        self.body = Some(body.as_bytes().to_vec());
        self
    }

    pub fn with_range(self, start: u64, end: Option<u64>) -> Self {
        let range_value = match end {
            Some(e) => format!("bytes={}-{}", start, e),
            None => format!("bytes={}-", start),
        };
        self.with_header("Range", &range_value)
    }

    pub fn with_user_agent(self, ua: &str) -> Self {
        self.with_header("User-Agent", ua)
    }

    pub fn with_referer(self, referer: &str) -> Self {
        self.with_header("Referer", referer)
    }

    pub fn with_accept_encoding(self, encoding: &str) -> Self {
        self.with_header("Accept-Encoding", encoding)
    }

    pub fn get_header(&self, name: &str) -> Option<&String> {
        self.headers
            .as_ref()?
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    }

    pub fn has_range(&self) -> bool {
        self.get_header("Range").is_some()
    }

    /// Serialize this request as an HTTP/1.1 message.
    ///
    /// `url` is emitted as supplied, allowing either origin-form targets or
    /// absolute-form targets for forward proxies.
    pub fn to_bytes(&self) -> Vec<u8> {
        let headers = self
            .headers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect::<Vec<_>>();
        serialize_http_request(&self.method, &self.url, &headers, self.body.as_deref())
    }
}

/// Serialize an HTTP/1.1 request from borrowed fields without copying its body.
pub fn serialize_http_request(
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Vec<u8> {
    let header_len = headers
        .iter()
        .map(|(name, value)| name.len() + value.len() + 4)
        .sum::<usize>();
    let head_len = method.len() + target.len() + 11 + header_len + 2;
    let mut bytes = Vec::with_capacity(head_len + body.map_or(0, <[u8]>::len));
    bytes.extend_from_slice(method.as_bytes());
    bytes.push(b' ');
    bytes.extend_from_slice(target.as_bytes());
    bytes.extend_from_slice(b" HTTP/1.1\r\n");
    for (name, value) in headers {
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(b": ");
        bytes.extend_from_slice(value.as_bytes());
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(b"\r\n");
    if let Some(body) = body {
        bytes.extend_from_slice(body);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::HttpRequest;

    #[test]
    fn serializes_origin_form_and_absolute_form_requests() {
        let origin = HttpRequest::get("/file.bin?download=1")
            .with_header("Host", "example.test")
            .to_bytes();
        assert_eq!(
            origin,
            b"GET /file.bin?download=1 HTTP/1.1\r\nHost: example.test\r\n\r\n"
        );

        let absolute = HttpRequest::get("ftp://example.test/file.bin")
            .with_header("Host", "example.test")
            .to_bytes();
        assert_eq!(
            absolute,
            b"GET ftp://example.test/file.bin HTTP/1.1\r\nHost: example.test\r\n\r\n"
        );
    }

    #[test]
    fn serializes_binary_request_body_without_text_conversion() {
        let request = HttpRequest::post("/upload")
            .with_header("Content-Length", "3")
            .with_body(vec![0, 0xff, b'x'])
            .to_bytes();
        assert!(request.ends_with(&[0, 0xff, b'x']));
    }
}
