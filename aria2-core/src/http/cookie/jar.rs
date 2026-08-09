//! Enhanced cookie jar (J4) with URL-based matching and SystemTime expiration.
//!
//! Provides `JarCookie` (an enhanced cookie representation using SystemTime)
//! and `CookieJar` (a collection manager for storing, matching, and
//! serializing cookies). Designed to work alongside the existing `Cookie`
//! from the parent module.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

use crate::error::{Aria2Error, Result};
use serde::{Deserialize, Serialize};

use super::jar_date::{format_systemtime_as_http_date, parse_http_date};

// ==================== JarCookie ====================

/// An enhanced cookie representation using SystemTime for expiration tracking.
///
/// This struct provides URL-based matching, Set-Cookie header parsing,
/// and serialization. It is designed to work alongside the existing `Cookie`
/// from the parent module while adding SystemTime-based expiration and simpler
/// URL-based matching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JarCookie {
    /// Cookie name
    pub name: String,
    /// Cookie value
    pub value: String,
    /// Domain this cookie belongs to (e.g., "example.com")
    pub domain: String,
    /// Path scope (usually "/")
    pub path: String,
    /// Expiration time; None means session cookie (no persistent expiry)
    pub expires: Option<SystemTime>,
    /// Only send over HTTPS connections
    pub secure: bool,
    /// Not accessible to JavaScript (client-side only)
    pub http_only: bool,
    /// When this cookie was created
    pub creation_time: SystemTime,
    /// SameSite policy carried by the canonical cookie model.
    pub same_site: super::SameSite,
}

impl JarCookie {
    /// Create a new basic session cookie.
    ///
    /// # Arguments
    ///
    /// * `name` - Cookie name
    /// * `value` - Cookie value
    /// * `domain` - Domain the cookie belongs to
    pub fn new(name: &str, value: &str, domain: &str) -> Self {
        Self {
            name: name.to_string(),
            value: value.to_string(),
            domain: domain.to_string(),
            path: "/".to_string(),
            expires: None,
            secure: false,
            http_only: false,
            creation_time: SystemTime::now(),
            same_site: super::SameSite::default(),
        }
    }

    /// Check whether this cookie should be sent for the given URL.
    ///
    /// Matching rules:
    /// 1. Secure cookies are only sent over HTTPS (`is_secure = true`)
    /// 2. The URL must contain the cookie's domain
    /// 3. The URL must match the cookie's path scope
    /// 4. The cookie must not have expired
    ///
    /// # Arguments
    ///
    /// * `url` - The request URL string to match against
    /// * `is_secure` - Whether the connection uses HTTPS
    ///
    /// # Returns
    ///
    /// `true` if this cookie should be included in requests to the given URL.
    pub fn matches_url(&self, url: &str, is_secure: bool) -> bool {
        let parsed = match Url::parse(url) {
            Ok(url) => url,
            Err(_) => return false,
        };
        let request_host = match parsed.host_str() {
            Some(host) => host.trim_end_matches('.').to_ascii_lowercase(),
            None => return false,
        };
        let cookie_domain = self
            .domain
            .trim()
            .trim_start_matches('.')
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if cookie_domain.is_empty() {
            return false;
        }

        if self.secure && (!is_secure || parsed.scheme() != "https") {
            return false;
        }
        if request_host != cookie_domain && !request_host.ends_with(&format!(".{cookie_domain}")) {
            return false;
        }

        let request_path = parsed.path();
        let cookie_path = if self.path.is_empty() || !self.path.starts_with('/') {
            "/"
        } else {
            self.path.as_str()
        };
        if !request_path.starts_with(cookie_path)
            || (cookie_path != "/"
                && request_path.len() > cookie_path.len()
                && !cookie_path.ends_with('/')
                && request_path.as_bytes()[cookie_path.len()] != b'/')
        {
            return false;
        }

        self.expires
            .is_none_or(|expires| SystemTime::now() <= expires)
    }

    /// Format this cookie as a Set-Cookie header value string.
    ///
    /// Produces output like: `name=value; Domain=example.com; Path=/`
    pub fn to_header_value(&self) -> String {
        let mut s = format!("{}={}", self.name, self.value);
        if !self.domain.is_empty() {
            s.push_str(&format!("; Domain={}", self.domain));
        }
        s.push_str(&format!("; Path={}", self.path));
        if let Some(_expires) = self.expires {
            // Use simplified date formatting for expires attribute
            if let Ok(_dur) = _expires.duration_since(SystemTime::UNIX_EPOCH) {
                // Approximate HTTP-date format
                s.push_str(&format!(
                    "; Expires={}",
                    format_systemtime_as_http_date(_expires)
                ));
            }
        }
        if self.secure {
            s.push_str("; Secure");
        }
        if self.http_only {
            s.push_str("; HttpOnly");
        }
        s
    }

    /// Parse a cookie from a Set-Cookie response header value.
    ///
    /// Supports standard attributes:
    /// - `Domain=` - cookie domain scope
    /// - `Path=` - cookie path scope
    /// - `Expires=` - expiration timestamp (RFC 7231 / RFC 850 / asctime formats)
    /// - `Secure` - HTTPS-only flag
    /// - `HttpOnly` - JavaScript-inaccessible flag
    /// - `Max-Age=` - relative expiration in seconds
    ///
    /// # Arguments
    ///
    /// * `header_value` - The raw Set-Cookie header value string
    ///
    /// # Returns
    ///
    /// `Some(JarCookie)` on successful parse, `None` if the header is malformed.
    ///
    /// # Example
    ///
    /// ```
    /// use aria2_core::http::cookie::JarCookie;
    ///
    /// let cookie = JarCookie::parse_set_cookie(
    ///     "session=abc123; Domain=example.com; Path=/; Secure; HttpOnly"
    /// ).unwrap();
    /// assert_eq!(cookie.name, "session");
    /// assert_eq!(cookie.domain, "example.com");
    /// assert!(cookie.secure);
    /// assert!(cookie.http_only);
    /// ```
    pub fn parse_set_cookie(header_value: &str) -> Option<Self> {
        let header = header_value.trim();
        if header.is_empty() {
            return None;
        }

        // Split into name=value part and attributes
        let parts: Vec<&str> = header.split(';').collect();
        if parts.is_empty() {
            return None;
        }

        // Parse name=value (first part before any semicolon)
        let nv: Vec<&str> = parts[0].trim().splitn(2, '=').collect();
        if nv.len() != 2 {
            return None;
        }

        let name = nv[0].trim();
        let value = nv[1].trim();
        if name.is_empty() {
            return None;
        }

        let mut cookie = Self::new(name, value, "");

        // Parse remaining attributes
        for attr in &parts[1..] {
            let attr = attr.trim();
            if attr.is_empty() {
                continue;
            }
            let kv: Vec<&str> = attr.splitn(2, '=').collect();
            match kv[0].trim().to_lowercase().as_str() {
                "domain" if kv.len() > 1 => {
                    cookie.domain = kv[1].trim().to_string();
                }
                "path" if kv.len() > 1 => {
                    cookie.path = kv[1].trim().to_string();
                }
                "max-age" if kv.len() > 1 => {
                    // Max-Age takes precedence over Expires per RFC 6265
                    if let Ok(secs) = kv[1].trim().parse::<u64>() {
                        cookie.expires = Some(SystemTime::now() + Duration::from_secs(secs));
                    }
                }
                "expires" if kv.len() > 1 => {
                    // Only set if not already set by Max-Age
                    if cookie.expires.is_none()
                        && let Ok(dt) = parse_http_date(kv[1].trim())
                    {
                        cookie.expires = Some(dt);
                    }
                }
                "secure" => {
                    cookie.secure = true;
                }
                "httponly" => {
                    cookie.http_only = true;
                }
                "samesite" if kv.len() > 1 => {
                    cookie.same_site = match kv[1].trim().to_ascii_lowercase().as_str() {
                        "strict" => super::SameSite::Strict,
                        "lax" => super::SameSite::Lax,
                        _ => super::SameSite::None,
                    };
                }
                _ => {} // Ignore unknown attributes
            }
        }

        Some(cookie)
    }
}

impl PartialEq for JarCookie {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.domain.eq_ignore_ascii_case(&other.domain)
            && self.path == other.path
    }
}

impl From<JarCookie> for super::Cookie {
    fn from(cookie: JarCookie) -> Self {
        cookie.into_cookie()
    }
}

impl JarCookie {
    /// Convert this persistence/API representation into the canonical storage cookie.
    pub fn into_cookie(self) -> super::Cookie {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let expiry_time = self
            .expires
            .and_then(|expiry| expiry.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);
        super::Cookie {
            name: self.name,
            value: self.value,
            domain: self.domain,
            path: self.path,
            expiry_time,
            creation_time: self
                .creation_time
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|duration| duration.as_secs() as i64)
                .unwrap_or(now),
            last_access_time: now,
            persistent: expiry_time != 0,
            host_only: false,
            secure: self.secure,
            http_only: self.http_only,
            same_site: self.same_site,
        }
    }
}

// ==================== CookieJar ====================

/// A cookie jar that stores cookies and provides URL-based matching for HTTP requests.
///
/// `CookieJar` manages a collection of `JarCookie` instances, supporting:
/// - Storing cookies received from Set-Cookie response headers
/// - Retrieving matching cookies for outgoing requests by URL
/// - Generating Cookie header strings from matching cookies
/// - Loading cookies from Netscape/Mozilla cookie file format
/// - Cleaning up expired cookies
///
/// # Example
///
/// ```rust,no_run
/// use aria2_core::http::cookie::{CookieJar, JarCookie};
///
/// let mut jar = CookieJar::new();
///
/// // Store a cookie from a Set-Cookie response header
/// if let Some(cookie) = JarCookie::parse_set_cookie("sid=abc; Domain=example.com") {
///     jar.store(cookie);
/// }
///
/// // Get cookies for an outgoing request
/// if let Some(header) = jar.cookie_header_for_url("https://example.com/api", true) {
///     println!("Cookie: {}", header); // "sid=abc"
/// }
/// ```
pub struct CookieJar {
    /// Internal cookie storage - made pub for session persistence serialization
    pub cookies: Vec<JarCookie>,
}

impl CookieJar {
    /// Create a new empty cookie jar.
    pub fn new() -> Self {
        Self {
            cookies: Vec::new(),
        }
    }

    /// Store a cookie received from a Set-Cookie response header.
    ///
    /// If a cookie with the same name, domain, and path already exists, it is replaced.
    /// This implements the update semantics defined in RFC 6265 Section 5.3.
    pub fn store(&mut self, cookie: JarCookie) {
        // Remove existing cookie with same name+domain (update semantics)
        self.cookies.retain(|c| {
            !(c.name == cookie.name
                && c.domain.eq_ignore_ascii_case(&cookie.domain)
                && c.path == cookie.path)
        });
        self.cookies.push(cookie);
    }

    /// Get all cookies that match the given URL for sending in a Cookie request header.
    ///
    /// Filters stored cookies based on domain, path, security, and expiration status.
    pub fn get_cookies_for_url(&self, url: &str, is_secure: bool) -> Vec<JarCookie> {
        self.cookies
            .iter()
            .filter(|c| c.matches_url(url, is_secure))
            .cloned()
            .collect()
    }

    /// Format all matching cookies as a Cookie header value string.
    ///
    /// Produces output like `"name1=val1; name2=val2"` suitable for the
    /// HTTP Cookie request header. Returns `None` if no cookies match.
    pub fn cookie_header_for_url(&self, url: &str, is_secure: bool) -> Option<String> {
        let matching = self.get_cookies_for_url(url, is_secure);
        if matching.is_empty() {
            return None;
        }
        let header: String = matching
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ");
        Some(header)
    }

    /// Remove all expired cookies from the jar.
    ///
    /// Session cookies (those without an expiration time) are never removed
    /// by this method — they persist until the session ends or explicitly deleted.
    ///
    /// # Returns
    ///
    /// The number of cookies that were removed.
    pub fn cleanup_expired(&mut self) -> usize {
        let before = self.cookies.len();
        self.cookies.retain(|c| {
            if let Some(exp) = c.expires {
                SystemTime::now() <= exp
            } else {
                true // No expiry = session cookie, keep it
            }
        });
        before - self.cookies.len()
    }

    /// Load cookies from a Netscape/Mozilla cookie file.
    ///
    /// The Netscape cookie file format uses tab-separated fields:
    /// ```text
    /// # Netscape HTTP Cookie File
    ///
    /// .example.com    TRUE    /    FALSE    0    session-cookie    value
    /// ```
    ///
    /// Fields: domain, include_subdomains, path, is_secure, expires_timestamp, name, value
    pub fn load_netscape_file(&mut self, path: &Path) -> Result<usize> {
        let content = fs::read_to_string(path).map_err(|e| Aria2Error::Io(e.to_string()))?;
        let mut loaded = 0;

        for line in content.lines() {
            // Skip comments, empty lines, and the header line
            if line.starts_with('#') || line.starts_with('\n') || line.is_empty() {
                continue;
            }

            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() >= 7 {
                // Parse the 7+ tab-separated fields
                let domain = fields[0].trim().to_ascii_lowercase();
                let path = fields[2].trim();
                let secure = fields[3] == "TRUE";
                // fields[4]: expires timestamp (Unix epoch), 0 = session cookie
                let expires = fields[4].trim().parse::<i64>().ok().and_then(|ts| {
                    if ts > 0 {
                        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(ts as u64))
                    } else {
                        None // Session cookie
                    }
                });
                let name = fields[5].trim().to_string();
                let value = if fields.len() > 7 {
                    // Value may contain tabs if there are extra fields; join remainder
                    fields[6..].join("\t")
                } else {
                    fields[6].trim().to_string()
                };

                let cookie = JarCookie {
                    name,
                    value,
                    domain: domain.to_string(),
                    path: path.to_string(),
                    expires,
                    secure,
                    http_only: false, // Netscape format doesn't track HttpOnly
                    creation_time: SystemTime::now(),
                    same_site: super::SameSite::default(),
                };

                self.cookies.push(cookie);
                loaded += 1;
            }
        }

        Ok(loaded)
    }

    /// Return the number of cookies currently stored in the jar.
    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    /// Check if the jar contains no cookies.
    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }

    /// Remove all cookies from the jar.
    pub fn clear(&mut self) {
        self.cookies.clear();
    }
}

impl Default for CookieJar {
    fn default() -> Self {
        Self::new()
    }
}
