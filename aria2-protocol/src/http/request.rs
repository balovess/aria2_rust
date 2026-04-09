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
}
