use super::{HttpClient, HttpResponseStream};
use crate::http::request::HttpRequest;
use crate::http::response::HttpResponse;

/// Validation failures returned by the fallible request-builder header API.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpRequestBuilderError {
    #[error("invalid header name: {0}")]
    InvalidName(String),
    #[error("invalid header value: {0}")]
    InvalidValue(String),
}

pub struct HttpRequestBuilder<'a> {
    client: &'a HttpClient,
    method: String,
    url: String,
    headers: Option<reqwest::header::HeaderMap>,
    body: Option<Vec<u8>>,
    error: Option<HttpRequestBuilderError>,
}

impl<'a> HttpRequestBuilder<'a> {
    pub(super) fn new(client: &'a HttpClient, method: &str, url: String) -> Self {
        Self {
            client,
            method: method.to_string(),
            url,
            headers: None,
            body: None,
            error: None,
        }
    }

    /// Add a header using the existing chainable API.
    ///
    /// Invalid input is reported by [`Self::send`] or
    /// [`Self::send_stream`]. Use [`Self::try_header`] when validation should
    /// happen immediately.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        if let Err(error) = self.insert_header(name, value) {
            self.error = Some(error);
        }
        self
    }

    /// Add a header and return validation errors instead of panicking.
    ///
    /// `header` remains available for trusted, compile-time-known headers and
    /// preserves the existing chainable API. Use this method for values that
    /// originate outside the application, such as user configuration or an
    /// RPC request.
    pub fn try_header(mut self, name: &str, value: &str) -> Result<Self, HttpRequestBuilderError> {
        self.insert_header(name, value)?;
        Ok(self)
    }

    fn insert_header(&mut self, name: &str, value: &str) -> Result<(), HttpRequestBuilderError> {
        let mut headers = self.headers.take().unwrap_or_default();
        let name = name
            .parse::<reqwest::header::HeaderName>()
            .map_err(|error| HttpRequestBuilderError::InvalidName(error.to_string()))?;
        let value = value
            .parse::<reqwest::header::HeaderValue>()
            .map_err(|error| HttpRequestBuilderError::InvalidValue(error.to_string()))?;
        headers.insert(name, value);
        self.headers = Some(headers);
        Ok(())
    }

    /// Add a typed header using the existing chainable API.
    ///
    /// Conversion failures are reported by [`Self::send`] or
    /// [`Self::send_stream`]. Use [`Self::try_header_raw`] for immediate
    /// validation.
    pub fn header_raw<K, V>(mut self, key: K, value: V) -> Self
    where
        K: TryInto<reqwest::header::HeaderName>,
        V: TryInto<reqwest::header::HeaderValue>,
    {
        let mut headers = self.headers.take().unwrap_or_default();
        match (key.try_into(), value.try_into()) {
            (Ok(k), Ok(v)) => {
                headers.insert(k, v);
                self.headers = Some(headers);
            }
            (Err(_), _) => {
                self.error = Some(HttpRequestBuilderError::InvalidName(
                    "header name conversion failed".to_string(),
                ));
            }
            (_, Err(_)) => {
                self.error = Some(HttpRequestBuilderError::InvalidValue(
                    "header value conversion failed".to_string(),
                ));
            }
        }
        self
    }

    /// Add a typed header and preserve conversion errors for the caller.
    pub fn try_header_raw<K, V>(mut self, key: K, value: V) -> Result<Self, HttpRequestBuilderError>
    where
        K: TryInto<reqwest::header::HeaderName>,
        K::Error: std::fmt::Display,
        V: TryInto<reqwest::header::HeaderValue>,
        V::Error: std::fmt::Display,
    {
        let key = key
            .try_into()
            .map_err(|error| HttpRequestBuilderError::InvalidName(error.to_string()))?;
        let value = value
            .try_into()
            .map_err(|error| HttpRequestBuilderError::InvalidValue(error.to_string()))?;
        let mut headers = self.headers.take().unwrap_or_default();
        headers.insert(key, value);
        self.headers = Some(headers);
        Ok(self)
    }

    pub fn range(self, start: u64, end: Option<u64>) -> Self {
        let range_value = match end {
            Some(e) => format!("bytes={}-{}", start, e),
            None => format!("bytes={}-", start),
        };
        self.header("Range", &range_value)
    }

    pub fn body<B: Into<Vec<u8>>>(mut self, body: B) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn user_agent(self, ua: &str) -> Self {
        self.header("User-Agent", ua)
    }

    pub fn referer(self, referer: &str) -> Self {
        self.header("Referer", referer)
    }

    fn into_request(self) -> Result<(&'a HttpClient, HttpRequest), HttpRequestBuilderError> {
        let Self {
            client,
            method,
            url,
            headers,
            body,
            error,
        } = self;
        if let Some(error) = error {
            return Err(error);
        }
        let headers_map = headers.map(|h| {
            h.iter()
                .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                .collect::<Vec<_>>()
        });

        Ok((
            client,
            HttpRequest {
                method,
                url,
                headers: headers_map,
                body,
            },
        ))
    }

    pub async fn send(self) -> Result<HttpResponse, String> {
        let (client, request) = self.into_request().map_err(|error| error.to_string())?;
        client.execute(request).await
    }

    /// Send this request while keeping the response body incremental.
    pub async fn send_stream(self) -> Result<HttpResponseStream, String> {
        let (client, request) = self.into_request().map_err(|error| error.to_string())?;
        client.execute_stream(request).await
    }
}
