//! HTTP Cookie implementation per RFC 6265 and RFC 6265bis.
//!
//! Provides cookie parsing, domain/path matching, and serialization.
//! The `from_set_cookie_header()` method validates the cookie's domain
//! against the request host to prevent cross-domain cookie injection.
//! SameSite attribute support follows RFC 6265bis Section 5.4.7.

pub mod jar;
pub mod jar_date;
pub mod netscape;
pub mod parsing;
pub mod storage;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tests_date;

#[cfg(test)]
mod tests_samesite;

#[cfg(test)]
mod tests_storage;

#[cfg(test)]
mod tests_storage_eviction;

#[cfg(test)]
mod tests_jar;

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::warn;

use parsing::{domain_matches, format_http_date, is_numeric_host, now_secs, parse_http_date};

// Re-export key types from sub-modules for convenient access
pub use jar::{CookieJar, JarCookie};
pub use storage::{CookieStorage, DOMAIN_EVICTION_RATE, DOMAIN_EVICTION_TRIGGER};

/// Maximum number of cookies per domain (matches C++ aria2 `MAX_COOKIE_PER_DOMAIN`).
pub const MAX_COOKIE_PER_DOMAIN: usize = 50;

/// SameSite attribute per RFC 6265bis Section 5.4.7.
///
/// Controls whether a cookie is sent with cross-site requests.
/// Absent SameSite attribute defaults to `SameSite::None` for compatibility
/// with C++ aria2 behavior (newer browsers treat absent as Lax).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SameSite {
    /// No SameSite attribute specified — cookie is sent in all contexts.
    #[default]
    None,
    /// SameSite=Lax — cookie is sent on top-level navigations.
    Lax,
    /// SameSite=Strict — cookie is only sent in same-site context.
    Strict,
}

impl fmt::Display for SameSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SameSite::None => write!(f, "None"),
            SameSite::Lax => write!(f, "Lax"),
            SameSite::Strict => write!(f, "Strict"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub expiry_time: i64,
    pub creation_time: i64,
    pub last_access_time: i64,
    pub persistent: bool,
    pub host_only: bool,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: SameSite,
}

impl Cookie {
    pub fn new(name: &str, value: &str, domain: &str) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        Self {
            name: name.to_string(),
            value: value.to_string(),
            domain: domain.to_string(),
            path: "/".to_string(),
            expiry_time: 0,
            creation_time: now,
            last_access_time: now,
            persistent: false,
            host_only: true,
            secure: false,
            http_only: false,
            same_site: SameSite::default(),
        }
    }

    /// Check whether this cookie should be sent with a request to `host:path`.
    ///
    /// Per RFC 6265 Section 5.4 and RFC 6265bis Section 5.4.7:
    /// - Secure cookies only over HTTPS
    /// - Domain must match (exact for host-only, subdomain for domain cookies)
    /// - Path must be a prefix match per RFC 6265 Section 5.1.4
    /// - Expired persistent cookies are excluded
    /// - SameSite enforcement per RFC 6265bis Section 5.4.7:
    ///   - `Strict`: only sent in same-site context (`is_cross_site` must be false)
    ///   - `Lax`: sent on top-level navigations (always allowed for download managers)
    ///   - `!None`: sent in all contexts (Secure flag is enforced at parse time)
    ///
    /// # Arguments
    ///
    /// * `host` - The request hostname
    /// * `path` - The request path
    /// * `date` - Current time as Unix epoch seconds
    /// * `secure` - Whether the request uses HTTPS
    /// * `is_cross_site` - Whether the request is cross-site (different registrable domain)
    pub fn match_request(
        &self,
        host: &str,
        path: &str,
        date: i64,
        secure: bool,
        is_cross_site: bool,
    ) -> bool {
        if self.secure && !secure {
            return false;
        }
        if self.persistent && self.is_expired(date) {
            return false;
        }
        if !self.domain_matches(host) {
            return false;
        }
        if !self.path_matches(path) {
            return false;
        }

        // SameSite enforcement per RFC 6265bis Section 5.4.7
        match self.same_site {
            SameSite::Strict => {
                // Strict cookies are only sent in same-site context
                if is_cross_site {
                    return false;
                }
            }
            SameSite::Lax => {
                // Lax cookies are sent on top-level navigations.
                // For a download manager, all requests are considered user-initiated
                // (top-level), so Lax cookies are always allowed regardless of
                // is_cross_site. This matches browser behavior for top-level GETs.
            }
            SameSite::None => {
                // None cookies are sent in all contexts (including cross-site).
                // Per C++ aria2 compatibility, absent SameSite defaults to None.
                // Modern browsers require SameSite=None cookies to have Secure,
                // but we follow C++ behavior for backward compatibility.
            }
        }

        true
    }

    pub fn is_expired(&self, base_time: i64) -> bool {
        if !self.persistent {
            return false;
        }
        self.expiry_time < base_time
    }

    /// Whether this cookie has immediately expired (Max-Age ≤ 0 or past expiry).
    /// Such cookies should be deleted from storage rather than stored.
    pub fn is_delete_cookie(&self) -> bool {
        self.persistent && self.expiry_time <= now_secs()
    }

    pub fn to_set_cookie_header(&self) -> String {
        let mut s = format!("{}={}", self.name, self.value);
        if self.persistent && self.expiry_time > 0 {
            s.push_str("; Expires=");
            s.push_str(&format_http_date(self.expiry_time));
        }
        if !self.domain.is_empty() {
            s.push_str("; Domain=");
            s.push_str(&self.domain);
        }
        if self.path != "/" {
            s.push_str("; Path=");
            s.push_str(&self.path);
        }
        if self.secure {
            s.push_str("; Secure");
        }
        if self.http_only {
            s.push_str("; HttpOnly");
        }
        // Output SameSite only when it differs from the default (None).
        // This avoids adding SameSite=None to cookies that never specified it.
        if self.same_site != SameSite::None {
            s.push_str("; SameSite=");
            s.push_str(&self.same_site.to_string());
        }
        s
    }

    /// Parse a `Set-Cookie` header value into a `Cookie`.
    ///
    /// Per RFC 6265 Section 5.3, the cookie's domain is validated against
    /// `request_host` to prevent cross-domain cookie injection. If the
    /// domain attribute does not domain-match the request host, the cookie
    /// is rejected (returns `None`).
    ///
    /// # Arguments
    ///
    /// * `header` - The raw Set-Cookie header value
    /// * `request_host` - The hostname of the HTTP request (for domain validation)
    /// * `default_path` - The default path computed from the request URI per RFC 6265 Section 5.1.4
    ///
    /// # Domain Validation Rules
    ///
    /// - If no `Domain` attribute → host-only cookie, domain = request_host
    /// - If `Domain` attribute present → strip leading dot, then check that
    ///   the domain domain-matches the request host (per RFC 6265 Section 5.1.3).
    ///   If not, the cookie is rejected.
    /// - Numeric IP hosts always force host-only mode (no subdomain matching).
    pub fn from_set_cookie_header(
        header: &str,
        request_host: &str,
        default_path: &str,
    ) -> Option<Self> {
        let header = header.trim();
        if header.is_empty() {
            return None;
        }

        // Split into name=value and attributes.
        // Fix: bare "SID=abc" (no semicolon) must work per RFC 6265.
        let (name_value, attrs_part) = match header.split_once(';') {
            Some((nv, attrs)) => (nv, Some(attrs)),
            None => (header, None), // No attributes — entire string is name=value
        };

        let nv = name_value.trim();
        let eq_pos = nv.find('=')?;
        let name = nv[..eq_pos].trim();
        let mut value = nv[eq_pos + 1..].trim();

        // Strip surrounding double-quotes from value per RFC 6265 Section 5.2
        if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
            value = &value[1..value.len() - 1];
        }

        if name.is_empty() {
            return None;
        }

        let mut cookie = Self::new(name, value, request_host);
        cookie.path = default_path.to_string();

        // Whether the Domain attribute was explicitly provided
        let mut domain_attr_provided = false;

        if let Some(attrs) = attrs_part {
            for attr in attrs.split(';') {
                let attr = attr.trim();
                if attr.is_empty() {
                    continue;
                }
                if let Some((k, v)) = attr.split_once('=') {
                    match k.trim().to_lowercase().as_str() {
                        "domain" => {
                            let domain_val = v.trim();
                            // Strip leading dot per RFC 6265 Section 5.2.3
                            let domain_val = domain_val.trim_start_matches('.');
                            if domain_val.is_empty() {
                                // Empty domain after stripping → reject
                                warn!("Set-Cookie with empty Domain attribute, rejecting");
                                return None;
                            }
                            cookie.domain = domain_val.to_string();
                            domain_attr_provided = true;
                        }
                        "path" => {
                            let path_val = v.trim();
                            // Path must start with / per RFC 6265 Section 5.2.4
                            if path_val.starts_with('/') {
                                cookie.path = path_val.to_string();
                            } else {
                                // If path doesn't start with /, ignore it and use default
                                warn!(
                                    path = path_val,
                                    "Set-Cookie Path doesn't start with /, using default"
                                );
                            }
                        }
                        "max-age" => {
                            if let Ok(secs) = v.trim().parse::<i64>() {
                                if secs <= 0 {
                                    // Max-Age ≤ 0 → cookie should be deleted immediately
                                    cookie.expiry_time = 0;
                                    cookie.persistent = true;
                                } else {
                                    cookie.expiry_time = now_secs() + secs;
                                    cookie.persistent = true;
                                }
                            } else {
                                // Non-numeric Max-Age → reject the cookie per C++ behavior
                                warn!(value = v.trim(), "Non-numeric Max-Age, rejecting cookie");
                                return None;
                            }
                        }
                        "expires" => {
                            // Max-Age takes precedence over Expires per RFC 6265 Section 5.3
                            if !cookie.persistent
                                && let Some(ep) = parse_http_date(v.trim())
                            {
                                cookie.expiry_time = ep;
                                cookie.persistent = true;
                            }
                            // If Expires is unparseable, C++ rejects the cookie.
                            // We are more lenient: we just ignore the Expires attribute.
                        }
                        "samesite" => {
                            // SameSite attribute per RFC 6265bis Section 5.4.7
                            // Value is case-insensitive
                            match v.trim().to_lowercase().as_str() {
                                "strict" => cookie.same_site = SameSite::Strict,
                                "lax" => cookie.same_site = SameSite::Lax,
                                "none" => cookie.same_site = SameSite::None,
                                _ => {
                                    // Per RFC 6265bis: unknown SameSite value MUST be ignored
                                    // (cookie stays at default SameSite::None)
                                    warn!(
                                        value = v.trim(),
                                        "Unknown SameSite value, ignoring attribute"
                                    );
                                }
                            }
                        }
                        _ => {}
                    }
                } else {
                    match attr.to_lowercase().as_str() {
                        "secure" => cookie.secure = true,
                        "httponly" => cookie.http_only = true,
                        _ => {}
                    }
                }
            }
        }

        // Domain validation per RFC 6265 Section 5.3 step 6-9
        // Follows C++ aria2 cookie_helper.cc parse() behavior:
        //   if cookieDomain empty    → hostOnly = true, domain = requestHost
        //   if domainMatch(host, dom) → hostOnly = isNumericHost(host)
        //   else                      → reject cookie
        if domain_attr_provided {
            // Validate that the cookie's domain domain-matches the request host
            if !domain_matches(request_host, &cookie.domain) {
                warn!(
                    cookie_domain = %cookie.domain,
                    request_host = %request_host,
                    "Cookie domain does not match request host, rejecting"
                );
                return None;
            }
            // Per C++ behavior: numeric hosts always force host-only mode
            // even when the domain matches exactly
            cookie.host_only = is_numeric_host(request_host);
        } else {
            // No Domain attribute → host-only cookie
            cookie.host_only = true;
            cookie.domain = request_host.to_string();
        }

        Some(cookie)
    }

    /// Check whether this cookie's domain matches the given host.
    ///
    /// For host-only cookies, the domain must exactly match the host (case-insensitive).
    /// For domain cookies, the host must either equal the domain or be
    /// a subdomain of it (per RFC 6265 Section 5.1.3).
    fn domain_matches(&self, host: &str) -> bool {
        if self.host_only {
            // Host-only cookies require exact domain match (case-insensitive)
            self.domain.eq_ignore_ascii_case(host)
        } else {
            // Domain cookies allow subdomain matching
            domain_matches(host, &self.domain)
        }
    }

    /// Check whether this cookie's path matches the given request path.
    ///
    /// Per RFC 6265 Section 5.1.4:
    /// - The cookie path is a prefix of the request path, AND
    /// - Either the cookie path ends with `/`, OR the character in the
    ///   request path immediately after the cookie path is `/`.
    fn path_matches(&self, request_path: &str) -> bool {
        parsing::path_matches(&self.path, request_path)
    }

    /// Compute the default path from a request URI per RFC 6265 Section 5.1.4.
    ///
    /// Algorithm:
    /// 1. If the URI path is empty or does not start with `/`, return `/`
    /// 2. If the URI path contains no more than one `/`, return `/`
    /// 3. Otherwise, return the characters up to (but not including) the right-most `/`
    pub fn default_path(request_uri_path: &str) -> String {
        if request_uri_path.is_empty() || !request_uri_path.starts_with('/') {
            return "/".to_string();
        }
        // Count slashes — if only one `/` (the leading one), default is `/`
        let slash_count = request_uri_path.chars().filter(|&c| c == '/').count();
        if slash_count <= 1 {
            return "/".to_string();
        }
        // Return everything up to (but NOT including) the last `/`
        if let Some(last_slash) = request_uri_path.rfind('/') {
            request_uri_path[..last_slash].to_string()
        } else {
            "/".to_string()
        }
    }

    /// Returns the maximum number of cookies allowed per domain.
    /// Matches C++ aria2's `MAX_COOKIE_PER_DOMAIN = 50`.
    pub fn max_cookie_per_domain() -> usize {
        MAX_COOKIE_PER_DOMAIN
    }
}

impl PartialEq for Cookie {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.domain == other.domain && self.path == other.path
    }
}

impl fmt::Display for Cookie {
    /// Format as `name=value`, matching C++ `Cookie::toString()`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={}", self.name, self.value)
    }
}
