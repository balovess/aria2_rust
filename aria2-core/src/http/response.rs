//! HTTP response parsing module
//!
//! Provides HTTP/1.1 response parsing with multi-value header support,
//! redirect detection, and streaming content decoding.

use std::collections::HashMap;
use url::Url;

use crate::error::{Aria2Error, Result};

/// HTTP response struct
///
/// Represents a complete HTTP response, including status code, reason phrase, version, headers, and optional body.
/// Supports multi-value headers (e.g., Set-Cookie).
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// Status code (e.g., 200, 404, 301)
    pub status_code: u16,
    /// Reason phrase (e.g., OK, Not Found, Moved Permanently)
    pub reason_phrase: String,
    /// HTTP version (e.g., "HTTP/1.1")
    pub version: String,
    /// Response headers (supports multi-value)
    pub headers: HashMap<String, Vec<String>>,
    /// Optional response body
    pub body: Option<Vec<u8>>,
}

impl HttpResponse {
    /// Parse HTTP response from raw bytes
    ///
    /// Parses response data conforming to the HTTP/1.1 specification, including status line, headers, and body.
    /// Supports multi-value headers (via comma separation or multiple headers with the same name).
    ///
    /// # Arguments
    ///
    /// * `data` - Raw HTTP response bytes
    ///
    /// # Returns
    ///
    /// Parsed HttpResponse, or an error message
    ///
    /// # Errors
    ///
    /// Returns an error if the response format is invalid or cannot be parsed
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let response_str = String::from_utf8(data.to_vec())
            .map_err(|e| Aria2Error::Parse(format!("Invalid UTF-8 in HTTP response: {}", e)))?;

        // Separate headers and body
        let (header_part, body_part) = match response_str.find("\r\n\r\n") {
            Some(pos) => (&response_str[..pos], &response_str[pos + 4..]),
            None => (response_str.as_str(), ""),
        };

        // Parse status line
        let mut lines = header_part.split("\r\n");
        let status_line = lines
            .next()
            .ok_or_else(|| Aria2Error::Parse("Empty HTTP response".to_string()))?;

        // Parse version, status_code, reason_phrase
        let parts: Vec<&str> = status_line.split_whitespace().collect();
        if parts.len() < 2 {
            return Err(Aria2Error::Parse(
                "Invalid HTTP status line format".to_string(),
            ));
        }

        let version = parts[0].to_string();
        let status_code: u16 = parts[1]
            .parse()
            .map_err(|e| Aria2Error::Parse(format!("Invalid status code: {}", e)))?;
        let reason_phrase = if parts.len() > 2 {
            parts[2..].join(" ")
        } else {
            String::new()
        };

        // Parse headers (supports multi-value)
        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            if let Some((key, value)) = line.split_once(':') {
                let key = key.trim().to_string();
                let value = value.trim().to_string();
                headers.entry(key).or_default().push(value);
            }
        }

        // Process body
        let body = if body_part.is_empty() {
            None
        } else {
            Some(body_part.as_bytes().to_vec())
        };

        Ok(HttpResponse {
            status_code,
            reason_phrase,
            version,
            headers,
            body,
        })
    }

    /// Get the first value of a specified header
    ///
    /// # Arguments
    ///
    /// * `name` - Header name (case-insensitive)
    ///
    /// # Returns
    ///
    /// Reference to the first header value, or None if not found
    pub fn header(&self, name: &str) -> Option<&String> {
        let name_lower = name.to_lowercase();
        for (key, values) in &self.headers {
            if key.to_lowercase() == name_lower {
                return values.first();
            }
        }
        None
    }

    /// Get all values of a specified header
    ///
    /// Particularly useful for headers like Set-Cookie that may appear multiple times.
    ///
    /// # Arguments
    ///
    /// * `name` - Header name (case-insensitive)
    ///
    /// # Returns
    ///
    /// Vector containing all matching values
    pub fn header_all(&self, name: &str) -> Vec<String> {
        let name_lower = name.to_lowercase();
        for (key, values) in &self.headers {
            if key.to_lowercase() == name_lower {
                return values.clone();
            }
        }
        Vec::new()
    }

    /// Get the value of the Content-Length header
    ///
    /// # Returns
    ///
    /// Content length (u64), or None if not present or parsing fails
    pub fn content_length(&self) -> Option<u64> {
        self.header("Content-Length")
            .and_then(|v| v.parse::<u64>().ok())
    }

    /// Check if this is a redirect response (3xx)
    ///
    /// # Returns
    ///
    /// true if the status code is in the 300-399 range
    pub fn is_redirect(&self) -> bool {
        (300..400).contains(&self.status_code)
    }

    /// Get the Location header and parse it as a URL
    ///
    /// Particularly useful for redirect responses. If it is a relative URL, it will be resolved based on the current request URL.
    ///
    /// # Returns
    ///
    /// Parsed absolute URL, or None if not present or parsing fails
    pub fn location(&self) -> Option<Url> {
        self.header("Location").and_then(|loc| Url::parse(loc).ok())
    }

    /// Get the decoded body using streaming decoders
    ///
    /// Automatically selects appropriate decoders based on the HTTP response's Content-Encoding and Transfer-Encoding headers
    /// to decode the response body. Supports GZip, Chunked, BZip2, and other encoding formats.
    ///
    /// Follows RFC 7230 Section 3.3.1: Transfer-Encoding takes precedence over Content-Encoding.
    ///
    /// # Returns
    ///
    /// Decoded raw data, or an error message. Returns an empty vector if no body is present.
    ///
    /// # Errors
    ///
    /// - If the encoding format is invalid or the data is corrupted
    /// - If an I/O error occurs during decoding
    pub fn decoded_body(&self) -> Result<Vec<u8>> {
        use crate::http::stream_filter::{AutoFilterSelector, process_filters};

        let encoding = self.header("Content-Encoding").map(|s| s.as_str());
        let transfer_enc = self.header("Transfer-Encoding").map(|s| s.as_str());

        let mut filters = AutoFilterSelector::select_filters(encoding, transfer_enc);

        match &self.body {
            Some(raw_data) => process_filters(&mut filters, raw_data),
            None => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_response_status_parsing() {
        // Test 200 OK
        let response_200 = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<body>";
        let resp = HttpResponse::from_bytes(response_200.as_bytes()).unwrap();
        assert_eq!(resp.status_code, 200);
        assert_eq!(resp.reason_phrase, "OK");
        assert_eq!(resp.version, "HTTP/1.1");

        // Test 404 Not Found
        let response_404 = "HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\n\r\nNot Found";
        let resp = HttpResponse::from_bytes(response_404.as_bytes()).unwrap();
        assert_eq!(resp.status_code, 404);
        assert_eq!(resp.reason_phrase, "Not Found");

        // Test 301 Moved Permanently
        let response_301 = "HTTP/1.1 301 Moved Permanently\r\nLocation: /new-url\r\n\r\n";
        let resp = HttpResponse::from_bytes(response_301.as_bytes()).unwrap();
        assert_eq!(resp.status_code, 301);
        assert_eq!(resp.reason_phrase, "Moved Permanently");
    }

    #[test]
    fn test_response_multi_value_headers() {
        let response = "HTTP/1.1 200 OK\r\n\
                       Set-Cookie: session=abc123; Path=/\r\n\
                       Set-Cookie: user=john; Domain=example.com\r\n\
                       Content-Type: text/html\r\n\r\n<body>";

        let resp = HttpResponse::from_bytes(response.as_bytes()).unwrap();

        // Test getting all Set-Cookie values
        let all_cookies = resp.header_all("Set-Cookie");
        assert_eq!(all_cookies.len(), 2);
        assert!(all_cookies.contains(&"session=abc123; Path=/".to_string()));
        assert!(all_cookies.contains(&"user=john; Domain=example.com".to_string()));

        // Test getting the first value
        let first_cookie = resp.header("Set-Cookie").unwrap();
        assert_eq!(first_cookie, "session=abc123; Path=/");
    }

    #[test]
    fn test_response_content_length() {
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 1024\r\n\r\n";
        let resp = HttpResponse::from_bytes(response.as_bytes()).unwrap();

        assert_eq!(resp.content_length(), Some(1024));

        // No Content-Length
        let response_no_cl = "HTTP/1.1 200 OK\r\n\r\n";
        let resp_no_cl = HttpResponse::from_bytes(response_no_cl.as_bytes()).unwrap();
        assert_eq!(resp_no_cl.content_length(), None);
    }

    #[test]
    fn test_response_is_redirect() {
        // Redirect status codes
        let redirect_resp = HttpResponse::from_bytes(
            "HTTP/1.1 301 Moved Permanently\r\nLocation: /new\r\n\r\n".as_bytes(),
        )
        .unwrap();
        assert!(redirect_resp.is_redirect());

        let redirect_302 =
            HttpResponse::from_bytes("HTTP/1.1 302 Found\r\n\r\n".as_bytes()).unwrap();
        assert!(redirect_302.is_redirect());

        // Non-redirect status codes
        let ok_resp = HttpResponse::from_bytes("HTTP/1.1 200 OK\r\n\r\n".as_bytes()).unwrap();
        assert!(!ok_resp.is_redirect());

        let error_resp =
            HttpResponse::from_bytes("HTTP/1.1 500 Internal Server Error\r\n\r\n".as_bytes())
                .unwrap();
        assert!(!error_resp.is_redirect());
    }

    #[test]
    fn test_response_location() {
        let response =
            "HTTP/1.1 301 Moved Permanently\r\nLocation: https://example.com/new-page\r\n\r\n";
        let resp = HttpResponse::from_bytes(response.as_bytes()).unwrap();

        let location = resp.location().unwrap();
        assert_eq!(location.as_str(), "https://example.com/new-page");
    }

    #[test]
    fn test_response_body_parsing() {
        let response =
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"status\":\"success\"}";
        let resp = HttpResponse::from_bytes(response.as_bytes()).unwrap();

        assert!(resp.body.is_some());
        assert_eq!(resp.body.unwrap(), b"{\"status\":\"success\"}");

        // No body
        let response_no_body = "HTTP/1.1 204 No Content\r\n\r\n";
        let resp_no_body = HttpResponse::from_bytes(response_no_body.as_bytes()).unwrap();
        assert!(resp_no_body.body.is_none());
    }

    #[test]
    fn test_response_case_insensitive_headers() {
        let response = "HTTP/1.1 200 OK\r\n\
                       Content-Type: text/html\r\n\
                       content-length: 100\r\n\r\n";

        let resp = HttpResponse::from_bytes(response.as_bytes()).unwrap();

        // Case-insensitive lookup
        assert!(resp.header("content-type").is_some());
        assert!(resp.header("CONTENT-TYPE").is_some());
        assert!(resp.header("Content-Length").is_some());
    }
}
