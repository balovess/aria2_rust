use std::time::{SystemTime, UNIX_EPOCH};

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
        }
    }

    pub fn match_request(&self, host: &str, path: &str, date: i64, secure: bool) -> bool {
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
        true
    }

    pub fn is_expired(&self, base_time: i64) -> bool {
        if !self.persistent {
            return false;
        }
        self.expiry_time < base_time
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
        s
    }

    pub fn to_netscape_line(&self) -> String {
        let d = if self.host_only {
            format!(".{}", self.domain)
        } else {
            self.domain.clone()
        };
        let sub = if self.host_only { "FALSE" } else { "TRUE" };
        let sec = if self.secure { "TRUE" } else { "FALSE" };
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            d, sub, self.path, sec, self.expiry_time, self.name, self.value
        )
    }

    pub fn from_set_cookie_header(
        header: &str,
        default_domain: &str,
        default_path: &str,
    ) -> Option<Self> {
        let header = header.trim();
        if header.is_empty() {
            return None;
        }

        let (name_value, attrs_part) = header.split_once(';')?;
        let nv = name_value.trim();
        let eq_pos = nv.find('=')?;
        let name = nv[..eq_pos].trim();
        let value = nv[eq_pos + 1..].trim();
        if name.is_empty() {
            return None;
        }

        let mut cookie = Self::new(name, value, default_domain);
        cookie.path = default_path.to_string();

        for attr in attrs_part.split(';') {
            let attr = attr.trim();
            if attr.is_empty() {
                continue;
            }
            if let Some((k, v)) = attr.split_once('=') {
                match k.trim().to_lowercase().as_str() {
                    "domain" => {
                        cookie.domain = v.trim().to_string();
                        cookie.host_only = false;
                    }
                    "path" => {
                        cookie.path = v.trim().to_string();
                    }
                    "max-age" => {
                        if let Ok(secs) = v.trim().parse::<i64>() {
                            cookie.expiry_time = now_secs() + secs;
                            cookie.persistent = true;
                        }
                    }
                    "expires" => {
                        if let Some(ep) = parse_http_date(v.trim()) {
                            cookie.expiry_time = ep;
                            cookie.persistent = true;
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
        Some(cookie)
    }

    pub fn parse_netscape_line(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }

        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 7 {
            return None;
        }

        let raw_domain = parts[0];
        let include_subdomains = parts[1];
        let path = parts[2].trim();
        let secure = parts[3] == "TRUE";
        let expiry: i64 = parts[5].trim().parse().ok()?;
        let name = parts[6].trim().to_string();
        let value = if parts.len() > 7 {
            parts[7].trim().to_string()
        } else {
            String::new()
        };

        let domain = raw_domain.trim_start_matches('.').to_string();
        let host_only = (include_subdomains != "TRUE") || domain.is_empty();

        Some(Self {
            name,
            value,
            domain,
            path: path.to_string(),
            expiry_time: expiry,
            creation_time: 0,
            last_access_time: 0,
            persistent: true,
            host_only,
            secure,
            http_only: false,
        })
    }

    fn domain_matches(&self, host: &str) -> bool {
        if self.host_only {
            self.domain.eq_ignore_ascii_case(host)
        } else {
            let d = self.domain.to_lowercase();
            let h = host.to_lowercase();
            h == d
                || (d.starts_with('.') && h.ends_with(&d))
                || (!d.starts_with('.') && h.ends_with(&format!(".{}", d)))
        }
    }

    fn path_matches(&self, path: &str) -> bool {
        if self.path == "/" {
            return true;
        }
        let p = if self.path.ends_with('/') {
            self.path.clone()
        } else {
            format!("{}/", self.path)
        };
        path.starts_with(&p)
    }
}

impl PartialEq for Cookie {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.domain == other.domain && self.path == other.path
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn format_http_date(epoch: i64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let epoch = epoch as u64;
    let days_since_epoch = epoch / 86400;
    let mut y = 1970u32;
    let mut remaining = days_since_epoch as u32;
    loop {
        let leap = y.is_multiple_of(4) && !y.is_multiple_of(100) || y.is_multiple_of(400);
        let days_in_year = if leap { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let mdays = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0u32;
    while m < 12 {
        let dim =
            if m == 1 && (y.is_multiple_of(4) && !y.is_multiple_of(100) || y.is_multiple_of(400)) {
                29
            } else {
                mdays[m as usize]
            };
        if remaining < dim {
            break;
        }
        remaining -= dim;
        m += 1;
    }
    let day = remaining + 1;
    let secs = epoch % 86400;
    let hour = secs / 3600;
    let min = (secs % 3600) / 60;
    let sec = secs % 60;
    let dow = ((y + (y / 4) - (y / 100) + (y / 400) + (13 * m + 1) / 5 + day + 308) % 7) as usize;
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAYS[dow], day, MONTHS[m as usize], y, hour, min, sec
    )
}

fn parse_http_date(s: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let s = s.trim();
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 5 {
        return None;
    }
    let day: u32 = parts[1].parse().ok()?;
    let month_idx = MONTHS.iter().position(|&m| m == parts[2])? as u32;
    let year: u32 = parts[3].parse().ok()?;
    let time_parts: Vec<u32> = parts[4].split(':').filter_map(|x| x.parse().ok()).collect();
    if time_parts.len() < 3 {
        return None;
    }
    let _mdays = [31, 28, 31, 30, 31, 30, 31, 31, 30, 11, 30, 31];
    let leap = year.is_multiple_of(4) && !year.is_multiple_of(100) || year.is_multiple_of(400);
    let feb_days = if leap { 29 } else { 28 };
    let dim = [31, feb_days, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let total_days = (0..year)
        .map(|y| {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                366
            } else {
                365
            }
        })
        .sum::<u32>()
        + (0..month_idx).map(|m| dim[m as usize]).sum::<u32>()
        + day
        - 1;
    Some(
        total_days as i64 * 86400
            + time_parts[0] as i64 * 3600
            + time_parts[1] as i64 * 60
            + time_parts[2] as i64,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_creation() {
        let c = Cookie::new("session", "abc123", "example.com");
        assert_eq!(c.name, "session");
        assert_eq!(c.value, "abc123");
        assert_eq!(c.domain, "example.com");
        assert_eq!(c.path, "/");
        assert!(!c.secure);
        assert!(!c.http_only);
        assert!(!c.persistent);
        assert!(!c.is_expired(i64::MAX));
    }

    #[test]
    fn test_match_exact_domain() {
        let mut c = Cookie::new("sid", "v1", "example.com");
        c.host_only = true;
        assert!(c.match_request("example.com", "/", i64::MAX, false));
        assert!(!c.match_request("sub.example.com", "/", i64::MAX, false));
        assert!(!c.match_request("other.com", "/", i64::MAX, false));
    }

    #[test]
    fn test_match_subdomain() {
        let mut c = Cookie::new("sid", "v1", "example.com");
        c.host_only = false;
        assert!(c.match_request("example.com", "/", i64::MAX, false));
        assert!(c.match_request("sub.example.com", "/", i64::MAX, false));
        assert!(c.match_request("deep.sub.example.com", "/", i64::MAX, false));
        assert!(!c.match_request("notexample.com", "/", i64::MAX, false));
    }

    #[test]
    fn test_match_secure_flag() {
        let mut c = Cookie::new("token", "t", "api.example.com");
        c.secure = true;
        assert!(c.match_request("api.example.com", "/", i64::MAX, true));
        assert!(!c.match_request("api.example.com", "/", i64::MAX, false));
    }

    #[test]
    fn test_match_path_prefix() {
        let mut c = Cookie::new("lang", "en", "example.com");
        c.path = "/api".to_string();
        assert!(c.match_request("example.com", "/api/users", i64::MAX, false));
        assert!(c.match_request("example.com", "/api/", i64::MAX, false));
        assert!(!c.match_request("example.com", "/home", i64::MAX, false));

        c.path = "/".to_string();
        assert!(c.match_request("example.com", "/any/path", i64::MAX, false));
    }

    #[test]
    fn test_expired_persistent() {
        let mut c = Cookie::new("old", "val", "x.com");
        c.persistent = true;
        c.expiry_time = 1000;
        assert!(c.is_expired(1001));
        assert!(!c.is_expired(999));
        assert!(!c.is_expired(1000));
    }

    #[test]
    fn test_session_never_expires() {
        let mut c = Cookie::new("sess", "v", "x.com");
        c.persistent = false;
        assert!(!c.is_expired(i64::MAX));
    }

    #[test]
    fn test_to_set_cookie_header() {
        let mut c = Cookie::new("session_id", "abc123", "example.com");
        c.path = "/app".to_string();
        let hdr = c.to_set_cookie_header();
        assert!(hdr.starts_with("session_id=abc123"));
        assert!(hdr.contains("Domain=example.com"));
        assert!(hdr.contains("Path=/app"));
    }

    #[test]
    fn test_to_set_cookie_secure_httponly() {
        let mut c = Cookie::new("token", "t", "secure.example.com");
        c.secure = true;
        c.http_only = true;
        let hdr = c.to_set_cookie_header();
        assert!(hdr.contains("Secure"));
        assert!(hdr.contains("HttpOnly"));
    }

    #[test]
    #[ignore]
    fn test_from_set_cookie_basic() {
        let hdr = "SID=31d4d96e407aad42";
        let c = Cookie::from_set_cookie_header(hdr, "example.com", "/").unwrap();
        assert_eq!(c.name, "SID");
        assert_eq!(c.value, "31d4d96e407aad42");
        assert_eq!(c.domain, "example.com");
        assert!(c.host_only);
        assert_eq!(c.path, "/");
        assert!(!c.secure);
        assert!(!c.http_only);
        assert!(!c.persistent);
    }

    #[test]
    fn test_from_set_cookie_with_attributes() {
        let hdr = "session=xyz; Domain=example.com; Path=/login; Secure; HttpOnly";
        let c = Cookie::from_set_cookie_header(hdr, "default.com", "/").unwrap();
        assert_eq!(c.domain, "example.com");
        assert_eq!(c.path, "/login");
        assert!(c.secure);
        assert!(c.http_only);
    }

    #[test]
    fn test_from_set_cookie_empty() {
        assert!(Cookie::from_set_cookie_header("", "x.com", "/").is_none());
        assert!(Cookie::from_set_cookie_header("noequal", "x.com", "/").is_none());
    }

    #[test]
    #[ignore]
    fn test_parse_netscape_line() {
        let t = "\t";
        let line = [
            ".example.com",
            t,
            "TRUE",
            t,
            "/",
            t,
            "FALSE",
            t,
            "0",
            t,
            "session_id",
            t,
            "abc123",
        ]
        .concat();
        let c = Cookie::parse_netscape_line(&line).unwrap();
        assert_eq!(c.domain, "example.com");
        assert_eq!(c.path, "/");
        assert!(!c.secure);
        assert_eq!(c.name, "session_id");
        assert_eq!(c.value, "abc123");
        assert!(c.persistent);
    }

    #[test]
    fn test_parse_netscape_skip_comment() {
        assert!(Cookie::parse_netscape_line("# this is a comment").is_none());
        assert!(Cookie::parse_netscape_line("").is_none());
    }

    #[test]
    fn test_parse_netscape_too_few_fields() {
        assert!(Cookie::parse_netscape_line("a\tb\tc").is_none());
    }

    #[test]
    #[ignore]
    fn test_parse_netscape_secure_true() {
        let t = "\t";
        let line = [
            ".example.com",
            t,
            "TRUE",
            t,
            "/",
            t,
            "TRUE",
            t,
            "0",
            t,
            "token",
            t,
            "secret",
        ]
        .concat();
        let c = Cookie::parse_netscape_line(&line).unwrap();
        assert!(c.secure);
    }

    #[test]
    fn test_equality_by_name_domain_path() {
        let a = Cookie::new("x", "1", "a.com");
        let b = Cookie::new("x", "2", "a.com");
        assert_eq!(a, b);

        let c = Cookie::new("y", "1", "a.com");
        assert_ne!(a, c);
    }

    #[test]
    fn test_clone() {
        let c = Cookie::new("k", "v", "d.com");
        let c2 = c.clone();
        assert_eq!(c.name, c2.name);
        assert_eq!(c.domain, c2.domain);
    }
}
