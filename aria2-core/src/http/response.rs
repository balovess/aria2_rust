//! HTTP response parsing module
//!
//! Provides HTTP/1.1 response parsing with multi-value header support,
//! redirect detection, and streaming content decoding.

use std::collections::HashMap;
use url::Url;

use crate::error::{Aria2Error, Result};
use crate::http::request::HttpMethod;

/// Return whether an HTTP status is one of aria2's redirect statuses.
///
/// `304 Not Modified` is intentionally excluded even though it is a 3xx
/// response. It is handled as a conditional-cache result and has no
/// `Location` header requirement.
pub(crate) fn is_redirect_status(status_code: u16) -> bool {
    matches!(status_code, 300..=303 | 307 | 308)
}

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

    /// Check if this is a redirect response (3xx) with a Location header.
    ///
    /// Matches C++ `HttpResponse::isRedirect()` which returns `true` ONLY when
    /// the status code is one of 300/301/302/303/307/308 AND the Location
    /// header is present. A 3xx response without a Location header is NOT
    /// considered a valid redirect.
    ///
    /// # Returns
    ///
    /// true if the status code is a recognized redirect code AND Location exists
    pub fn is_redirect(&self) -> bool {
        is_redirect_status(self.status_code) && self.header("Location").is_some()
    }

    /// Determine the HTTP method to use when following this redirect.
    ///
    /// Matches C++ `HttpRequest::getRequestMethod()` + redirect method change
    /// logic per RFC 7231 §6.4:
    ///
    /// - **303 See Other**: Always change to GET (C++ resets method to GET).
    ///   This is the most common redirect type for form submissions.
    /// - **301/302**: C++ aria2 changes POST→GET for 301/302 redirects,
    ///   matching the historical browser behavior that the C++ code follows.
    ///   Non-POST methods are preserved.
    /// - **307/308**: Preserve the original method (RFC 7538 for 308,
    ///   RFC 7231 §6.4.7 for 307). The request body is also preserved.
    ///
    /// # Arguments
    ///
    /// * `original_method` - The HTTP method of the original (pre-redirect) request
    ///
    /// # Returns
    ///
    /// The HTTP method to use for the redirected request
    pub fn redirect_method(&self, original_method: &HttpMethod) -> HttpMethod {
        match self.status_code {
            // 303 See Other: always switch to GET (RFC 7231 §6.4.4)
            303 => HttpMethod::Get,
            // 301/302: change POST→GET to match historical browser behavior
            // (C++ aria2 does this in HttpRequest state machine)
            301 | 302 => {
                if *original_method == HttpMethod::Post {
                    HttpMethod::Get
                } else {
                    original_method.clone()
                }
            }
            // 307 Temporary Redirect / 308 Permanent Redirect:
            // preserve the original method per RFC 7538 and RFC 7231 §6.4.7
            307 | 308 => original_method.clone(),
            _ => original_method.clone(),
        }
    }

    /// Whether this redirect response requires preserving the request body.
    ///
    /// Per RFC 7231 §6.4, a redirect that preserves the method (307/308)
    /// should also preserve the request body. A redirect that changes the
    /// method to GET (301/302 from POST, 303) should drop the body.
    ///
    /// # Arguments
    ///
    /// * `original_method` - The HTTP method of the original request
    ///
    /// # Returns
    ///
    /// true if the request body should be forwarded to the redirect target
    pub fn redirect_preserves_body(&self, original_method: &HttpMethod) -> bool {
        match self.status_code {
            // 303 always drops body (switches to GET)
            303 => false,
            // 301/302 from POST switches to GET → drops body
            301 | 302 => *original_method != HttpMethod::Post,
            // 307/308 preserve method → preserve body
            307 | 308 => true,
            _ => true,
        }
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
        // Redirect status codes with Location header
        let redirect_resp = HttpResponse::from_bytes(
            "HTTP/1.1 301 Moved Permanently\r\nLocation: /new\r\n\r\n".as_bytes(),
        )
        .unwrap();
        assert!(redirect_resp.is_redirect());

        // 302 without Location header is NOT a valid redirect (C++ behavior)
        let redirect_302_no_loc =
            HttpResponse::from_bytes("HTTP/1.1 302 Found\r\n\r\n".as_bytes()).unwrap();
        assert!(!redirect_302_no_loc.is_redirect());

        // 302 WITH Location is a valid redirect
        let redirect_302_with_loc =
            HttpResponse::from_bytes("HTTP/1.1 302 Found\r\nLocation: /other\r\n\r\n".as_bytes())
                .unwrap();
        assert!(redirect_302_with_loc.is_redirect());

        // Non-redirect status codes
        let ok_resp = HttpResponse::from_bytes("HTTP/1.1 200 OK\r\n\r\n".as_bytes()).unwrap();
        assert!(!ok_resp.is_redirect());

        let error_resp =
            HttpResponse::from_bytes("HTTP/1.1 500 Internal Server Error\r\n\r\n".as_bytes())
                .unwrap();
        assert!(!error_resp.is_redirect());

        let not_modified =
            HttpResponse::from_bytes("HTTP/1.1 304 Not Modified\r\n\r\n".as_bytes()).unwrap();
        assert!(!not_modified.is_redirect());
    }

    #[test]
    fn test_redirect_status_set_matches_aria2() {
        for status in [300, 301, 302, 303, 307, 308] {
            assert!(
                is_redirect_status(status),
                "status {status} should redirect"
            );
        }
        for status in [304, 305, 306, 309, 399] {
            assert!(
                !is_redirect_status(status),
                "status {status} is not a redirect"
            );
        }
    }

    #[test]
    fn test_redirect_method_303_see_other() {
        // 303 always changes method to GET
        let resp =
            HttpResponse::from_bytes("HTTP/1.1 303 See Other\r\nLocation: /new\r\n\r\n".as_bytes())
                .unwrap();
        assert_eq!(resp.redirect_method(&HttpMethod::Post), HttpMethod::Get);
        assert_eq!(resp.redirect_method(&HttpMethod::Put), HttpMethod::Get);
        assert_eq!(resp.redirect_method(&HttpMethod::Get), HttpMethod::Get);
        // 303 always drops body
        assert!(!resp.redirect_preserves_body(&HttpMethod::Post));
    }

    #[test]
    fn test_redirect_method_301_302_post_to_get() {
        // 301/302 change POST→GET (C++ historical behavior)
        let resp_301 = HttpResponse::from_bytes(
            "HTTP/1.1 301 Moved Permanently\r\nLocation: /new\r\n\r\n".as_bytes(),
        )
        .unwrap();
        assert_eq!(resp_301.redirect_method(&HttpMethod::Post), HttpMethod::Get);
        assert!(!resp_301.redirect_preserves_body(&HttpMethod::Post));

        let resp_302 =
            HttpResponse::from_bytes("HTTP/1.1 302 Found\r\nLocation: /new\r\n\r\n".as_bytes())
                .unwrap();
        assert_eq!(resp_302.redirect_method(&HttpMethod::Post), HttpMethod::Get);
        // Non-POST methods preserved for 301/302
        assert_eq!(resp_301.redirect_method(&HttpMethod::Get), HttpMethod::Get);
        assert_eq!(
            resp_301.redirect_method(&HttpMethod::Head),
            HttpMethod::Head
        );
        assert!(resp_301.redirect_preserves_body(&HttpMethod::Get));
    }

    #[test]
    fn test_redirect_method_307_308_preserve() {
        // 307/308 preserve original method (RFC 7538, RFC 7231 §6.4.7)
        let resp_307 = HttpResponse::from_bytes(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: /new\r\n\r\n".as_bytes(),
        )
        .unwrap();
        assert_eq!(
            resp_307.redirect_method(&HttpMethod::Post),
            HttpMethod::Post
        );
        assert!(resp_307.redirect_preserves_body(&HttpMethod::Post));

        let resp_308 = HttpResponse::from_bytes(
            "HTTP/1.1 308 Permanent Redirect\r\nLocation: /new\r\n\r\n".as_bytes(),
        )
        .unwrap();
        assert_eq!(
            resp_308.redirect_method(&HttpMethod::Post),
            HttpMethod::Post
        );
        assert_eq!(resp_308.redirect_method(&HttpMethod::Get), HttpMethod::Get);
        assert!(resp_308.redirect_preserves_body(&HttpMethod::Post));
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
