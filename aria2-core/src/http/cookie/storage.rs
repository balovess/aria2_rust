//! Thread-safe cookie storage using RwLock for concurrent access.
//!
//! This module provides `CookieStorage`, the original cookie storage that works
//! with the `Cookie` struct from the parent module, providing add/find/expire
//! operations with host+path matching per RFC 6265.

use std::fs;
use std::path::Path;
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Aria2Error, Result};
use crate::http::ns_cookie_parser::NsCookieParser;

use super::Cookie;

/// Thread-safe cookie storage using RwLock for concurrent access.
///
/// This is the original cookie storage that works with the `Cookie` struct
/// from `cookie.rs`, providing add/find/expire operations with host+path matching.
pub struct CookieStorage {
    cookies: RwLock<Vec<Cookie>>,
}

impl CookieStorage {
    pub fn new() -> Self {
        Self {
            cookies: RwLock::new(Vec::new()),
        }
    }

    /// Add a cookie to storage. If a cookie with the same name+domain+path
    /// already exists, it is replaced (preserving creation_time per RFC 6265).
    ///
    /// If the cookie is a "delete cookie" (Max-Age ≤ 0 or already expired),
    /// the existing cookie is removed instead.
    ///
    /// Enforces `MAX_COOKIE_PER_DOMAIN` limit: when exceeded, expired cookies
    /// are purged first; if still over limit, the least-recently-accessed
    /// cookie for that domain is evicted (matching C++ behavior).
    pub fn add(&self, cookie: Cookie) {
        let mut cookies = self.cookies.write().unwrap_or_else(|e| e.into_inner());

        // Check for existing cookie with same name+domain+path
        if let Some(pos) = cookies.iter().position(|c| c == &cookie) {
            if cookie.is_delete_cookie() {
                // Delete cookie: remove the existing entry
                cookies.remove(pos);
                return;
            }
            // Preserve creation_time from the existing cookie per RFC 6265 Section 5.3
            let mut updated = cookie;
            updated.creation_time = cookies[pos].creation_time;
            cookies[pos] = updated;
            return;
        }

        // If this is a delete cookie with no existing match, just skip it
        if cookie.is_delete_cookie() {
            return;
        }

        // Enforce max cookies per domain
        let domain = cookie.domain.clone();
        let domain_count = cookies.iter().filter(|c| c.domain == domain).count();
        if domain_count >= Cookie::max_cookie_per_domain() {
            // First try to evict expired cookies for this domain
            let before = cookies.len();
            cookies.retain(|c| {
                c.domain != domain
                    || !c.is_expired(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs() as i64,
                    )
            });
            if cookies.len() == before {
                // No expired cookies to evict; remove least-recently-accessed for this domain
                if let Some(lru_pos) = cookies
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.domain == domain)
                    .min_by_key(|(_, c)| c.last_access_time)
                    .map(|(i, _)| i)
                {
                    cookies.remove(lru_pos);
                }
            }
        }

        cookies.push(cookie);
    }

    /// Find all cookies matching the given host, path, and security context.
    ///
    /// Per RFC 6265 Section 5.4, cookies are sorted by:
    /// 1. Path depth descending (longer/more specific paths first)
    /// 2. Creation time ascending (earlier-created cookies first)
    ///
    /// # Arguments
    ///
    /// * `host` - The request hostname
    /// * `path` - The request path
    /// * `secure` - Whether the request uses HTTPS
    /// * `is_cross_site` - Whether the request is cross-site (for SameSite enforcement)
    pub fn find_cookies(
        &self,
        host: &str,
        path: &str,
        secure: bool,
        is_cross_site: bool,
    ) -> Vec<Cookie> {
        let date = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // Read lock: collect matching cookies
        let cookies = self.cookies.read().unwrap_or_else(|e| e.into_inner());
        let mut matching: Vec<Cookie> = cookies
            .iter()
            .filter(|c| c.match_request(host, path, date, secure, is_cross_site))
            .cloned()
            .collect();
        drop(cookies); // Release read lock

        // Write lock: update last_access_time for matched cookies
        let mut cookies = self.cookies.write().unwrap_or_else(|e| e.into_inner());
        for c in cookies.iter_mut() {
            if c.match_request(host, path, date, secure, is_cross_site) {
                c.last_access_time = date;
            }
        }

        // Sort per RFC 6265 Section 5.4: longer paths first, then earlier creation
        matching.sort_by(|a, b| {
            let depth_a = a.path.matches('/').count();
            let depth_b = b.path.matches('/').count();
            depth_b
                .cmp(&depth_a)
                .then_with(|| a.creation_time.cmp(&b.creation_time))
        });

        matching
    }

    pub fn find_cookies_for_url(&self, url: &reqwest::Url) -> Vec<Cookie> {
        self.find_cookies_for_url_with_context(url, false)
    }

    /// Find cookies for a URL with explicit cross-site context.
    ///
    /// This is the full-featured version that allows specifying the SameSite
    /// context. Use `find_cookies_for_url()` for the common case of same-site
    /// requests (the default for download manager initiated downloads).
    pub fn find_cookies_for_url_with_context(
        &self,
        url: &reqwest::Url,
        is_cross_site: bool,
    ) -> Vec<Cookie> {
        let host = url.host_str().unwrap_or("");
        let path = url.path();
        let scheme = url.scheme();
        let secure = scheme == "https";
        self.find_cookies(host, path, secure, is_cross_site)
    }

    pub fn expire_cookies(&self, base_time: i64) -> usize {
        let mut cookies = self.cookies.write().unwrap_or_else(|e| e.into_inner());
        let before = cookies.len();
        cookies.retain(|c| !c.is_expired(base_time));
        before - cookies.len()
    }

    pub fn count(&self) -> usize {
        self.cookies.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn load_file(&self, path: &Path) -> Result<usize> {
        let data = fs::read_to_string(path).map_err(|e| Aria2Error::Io(e.to_string()))?;
        let parsed = NsCookieParser::parse_str(&data)?;
        let n = parsed.len();
        let mut cookies = self.cookies.write().unwrap_or_else(|e| e.into_inner());
        for cookie in parsed {
            if let Some(pos) = cookies.iter().position(|c| *c == cookie) {
                cookies[pos] = cookie;
            } else {
                cookies.push(cookie);
            }
        }
        Ok(n)
    }

    pub fn save_file(&self, path: &Path) -> Result<()> {
        let cookies = self.cookies.read().unwrap_or_else(|e| e.into_inner());
        let mut lines = Vec::with_capacity(cookies.len() + 3);
        lines.push("# Netscape HTTP Cookie File".to_string());
        lines.push("# This file is generated by aria2-rust".to_string());
        for c in cookies.iter() {
            lines.push(c.to_netscape_line());
        }
        fs::write(path, lines.join("\n")).map_err(|e| Aria2Error::Io(e.to_string()))?;
        Ok(())
    }

    pub fn clear(&self) {
        self.cookies
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    pub fn to_header_string(&self, host: &str, path: &str, secure: bool) -> String {
        self.to_header_string_with_context(host, path, secure, false)
    }

    /// Format matching cookies as a Cookie header value string with SameSite context.
    ///
    /// Same as `to_header_string()` but allows specifying the cross-site context
    /// for SameSite enforcement.
    pub fn to_header_string_with_context(
        &self,
        host: &str,
        path: &str,
        secure: bool,
        is_cross_site: bool,
    ) -> String {
        let cookies = self.find_cookies(host, path, secure, is_cross_site);
        if cookies.is_empty() {
            return String::new();
        }
        cookies
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub fn is_empty(&self) -> bool {
        self.cookies
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}

impl Default for CookieStorage {
    fn default() -> Self {
        Self::new()
    }
}
