//! Netscape/Mozilla cookies.txt format serialization and deserialization.
//!
//! Handles reading and writing cookies in the classic Netscape cookie file
//! format used by Firefox, curl, and wget. SameSite and HttpOnly attributes
//! are encoded per Firefox conventions in the domain field.

use tracing::warn;

use super::{Cookie, SameSite};
use super::parsing::is_numeric_host;

impl Cookie {
    /// Serialize cookie in Netscape/Mozilla cookies.txt format.
    ///
    /// SameSite information is encoded as a suffix on the domain field
    /// per Firefox cookie file conventions:
    /// - None (or absent, the default): no suffix
    /// - Strict: domain suffixed with `#SameSite=1`
    /// - Lax: domain suffixed with `#SameSite=2`
    pub fn to_netscape_line(&self) -> String {
        let d = if self.host_only {
            self.domain.clone()
        } else {
            format!(".{}", self.domain)
        };
        let sub = if self.host_only { "FALSE" } else { "TRUE" };
        let sec = if self.secure { "TRUE" } else { "FALSE" };

        // Encode HttpOnly and SameSite in domain field per Firefox format.
        // Firefox uses `#HttpOnly_` prefix for http_only and `#SameSite=N` suffix.
        // Only encode SameSite when it differs from the default (None) to keep
        // backward compatibility with existing cookie files.
        let mut domain_field = String::new();
        if self.http_only {
            domain_field.push_str("#HttpOnly_");
        }
        domain_field.push_str(&d);
        match self.same_site {
            SameSite::None => {} // Default — no suffix needed
            SameSite::Strict => domain_field.push_str("#SameSite=1"),
            SameSite::Lax => domain_field.push_str("#SameSite=2"),
        }

        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            domain_field, sub, self.path, sec, self.expiry_time, self.name, self.value
        )
    }

    /// Parse a Netscape/Mozilla cookies.txt format line into a `Cookie`.
    ///
    /// Expected field order (tab-separated):
    /// `domain  include_subdomains  path  secure  expiry  name  value`
    ///
    /// Firefox extensions in the domain field are supported:
    /// - `#HttpOnly_` prefix → `http_only = true`
    /// - `#SameSite=1` suffix → `SameSite::Strict`
    /// - `#SameSite=2` suffix → `SameSite::Lax`
    ///
    /// Returns `None` if the line is malformed, a comment, or empty.
    pub fn parse_netscape_line(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }

        // Skip comment lines (starting with #), but NOT lines like
        // `#HttpOnly_example.com` which encode HttpOnly in the domain field.
        if line.starts_with('#') && !line.starts_with("#HttpOnly_") {
            return None;
        }

        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 7 {
            return None;
        }

        let raw_domain = parts[0].trim();
        let include_subdomains = parts[1].trim();
        let path = parts[2].trim();
        let secure = parts[3] == "TRUE";
        // Field indices per Netscape cookie format:
        // 0: domain, 1: include_subdomains, 2: path, 3: secure,
        // 4: expiry_time, 5: name, 6: value
        let expiry: i64 = parts[4].trim().parse().ok()?;
        let name = parts[5].trim().to_string();
        let value = if parts.len() > 6 {
            parts[6].trim().to_string()
        } else {
            String::new()
        };

        if name.is_empty() {
            return None;
        }

        // Parse Firefox extensions in domain field:
        // - `#HttpOnly_` prefix → http_only = true
        // - `#SameSite=1` suffix → Strict
        // - `#SameSite=2` suffix → Lax
        // - No suffix → None (default per C++ aria2 compatibility)
        let mut http_only = false;
        let mut same_site = SameSite::None;
        let mut domain_field = raw_domain;

        if domain_field.starts_with("#HttpOnly_") {
            http_only = true;
            domain_field = &domain_field["#HttpOnly_".len()..];
        }

        if let Some(ss_pos) = domain_field.find("#SameSite=") {
            let ss_val = &domain_field[ss_pos + "#SameSite=".len()..];
            domain_field = &domain_field[..ss_pos];
            match ss_val {
                "1" => same_site = SameSite::Strict,
                "2" => same_site = SameSite::Lax,
                _ => {
                    warn!(value = ss_val, "Unknown SameSite value in Netscape format, treating as None");
                }
            }
        }

        // Per C++ NsCookieParser: reject if domain is empty after stripping leading dot
        let domain = domain_field.trim_start_matches('.').to_string();
        if domain.is_empty() {
            return None;
        }

        // Per C++ NsCookieParser: reject if path doesn't start with /
        if !path.starts_with('/') {
            return None;
        }

        // Per C++ NsCookieParser: hostOnly = isNumericHost || (include_subdomains != "TRUE")
        let host_only = is_numeric_host(&domain) || include_subdomains != "TRUE";

        // Per C++ NsCookieParser: expiryTime == 0 means session cookie
        let persistent = expiry != 0;

        Some(Self {
            name,
            value,
            domain,
            path: path.to_string(),
            expiry_time: expiry,
            creation_time: 0,
            last_access_time: 0,
            persistent,
            host_only,
            secure,
            http_only,
            same_site,
        })
    }
}
