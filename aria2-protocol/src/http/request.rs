#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Option<Vec<(String, String)>>,
    pub body: Option<Vec<u8>>,
}

/// Failure to serialize invalid HTTP/1.1 request fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HttpRequestSerializationError {
    /// The method contains bytes outside the HTTP token grammar.
    #[error("invalid HTTP method")]
    InvalidMethod,
    /// The request target contains whitespace or control bytes.
    #[error("invalid HTTP request target")]
    InvalidTarget,
    /// A field name contains bytes outside the HTTP token grammar.
    #[error("invalid HTTP field name")]
    InvalidFieldName,
    /// A field value contains forbidden control bytes.
    #[error("invalid HTTP field value")]
    InvalidFieldValue,
}

/// Return whether a value can be serialized as an HTTP field value.
///
/// Horizontal tab, visible ASCII, and obs-text octets are permitted. Other
/// control bytes, including CR and LF, are rejected to prevent field injection.
pub fn is_valid_http_field_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte == b'\t' || (byte >= 0x20 && byte != 0x7f))
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
    /// absolute-form targets for forward proxies. This is a raw compatibility
    /// serializer; use [`Self::try_to_bytes`] for network transmission.
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

    /// Validate and serialize this request for use on a network connection.
    pub fn try_to_bytes(&self) -> Result<Vec<u8>, HttpRequestSerializationError> {
        let headers = self
            .headers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect::<Vec<_>>();
        try_serialize_http_request(&self.method, &self.url, &headers, self.body.as_deref())
    }
}

/// Validate and serialize an HTTP/1.1 request from borrowed fields.
pub fn try_serialize_http_request(
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Result<Vec<u8>, HttpRequestSerializationError> {
    validate_http_request(method, target, headers)?;
    Ok(serialize_http_request(method, target, headers, body))
}

/// Validate HTTP/1.1 request-line and field components without allocating a
/// serialized request buffer.
pub fn validate_http_request(
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
) -> Result<(), HttpRequestSerializationError> {
    if !is_http_token(method) {
        return Err(HttpRequestSerializationError::InvalidMethod);
    }
    if target.is_empty() || !target.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
        return Err(HttpRequestSerializationError::InvalidTarget);
    }
    for (name, value) in headers {
        if !is_http_token(name) {
            return Err(HttpRequestSerializationError::InvalidFieldName);
        }
        if !is_valid_http_field_value(value) {
            return Err(HttpRequestSerializationError::InvalidFieldValue);
        }
    }
    Ok(())
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// Serialize an HTTP/1.1 request from borrowed fields without copying its body.
///
/// This is a raw compatibility primitive. Use [`try_serialize_http_request`]
/// for request data that will be sent over a network connection.
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
    use super::{
        HttpRequest, HttpRequestSerializationError, is_valid_http_field_value,
        try_serialize_http_request, validate_http_request,
    };

    #[test]
    fn validates_http_field_value_control_bytes() {
        assert!(is_valid_http_field_value("agent/1.0\t(comment)"));
        assert!(is_valid_http_field_value("agent-\u{80}"));
        for invalid in ["agent\r\nX-Injected: yes", "agent\0", "agent\u{7f}"] {
            assert!(!is_valid_http_field_value(invalid), "accepted {invalid:?}");
        }
    }

    #[test]
    fn validated_serializer_rejects_request_line_and_field_injection() {
        assert_eq!(
            try_serialize_http_request("GET\r\nX", "/file", &[], None),
            Err(HttpRequestSerializationError::InvalidMethod)
        );
        assert_eq!(
            try_serialize_http_request("GET", "/file\r\nX: y", &[], None),
            Err(HttpRequestSerializationError::InvalidTarget)
        );
        assert_eq!(
            try_serialize_http_request("GET", "/file", &[("X-Test\r\nX", "v")], None),
            Err(HttpRequestSerializationError::InvalidFieldName)
        );
        assert_eq!(
            try_serialize_http_request("GET", "/file", &[("X-Test", "v\r\nX: y")], None),
            Err(HttpRequestSerializationError::InvalidFieldValue)
        );
        assert_eq!(validate_http_request("GET", "/file", &[]), Ok(()));
    }

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
