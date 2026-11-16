//! FTP and FTPS URI syntax parsing.

pub use crate::uri::percent_decode;
use crate::uri::percent_encode;

/// Parsed FTP URI fields before engine defaults are applied.
#[derive(Clone, PartialEq, Eq)]
pub struct FtpUri {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub remote_path: String,
    pub secure: bool,
}

impl std::fmt::Debug for FtpUri {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FtpUri")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username.as_ref().map(|_| "[REDACTED]"))
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field("remote_path", &self.remote_path)
            .field("secure", &self.secure)
            .finish()
    }
}

/// Percent-encode a URI user-info component using the RFC 3986 unreserved set.
pub fn percent_encode_userinfo(value: &str) -> String {
    percent_encode(value)
}

/// Parse the FTP URI forms accepted by the download engine.
///
/// The parser intentionally preserves the historical permissive port handling:
/// malformed ports fall back to the protocol's default port.
pub fn parse_uri(uri: &str) -> Option<FtpUri> {
    let (scheme_len, secure, default_port) = if uri
        .get(..6)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("ftp://"))
    {
        (6, false, super::DEFAULT_PORT)
    } else if uri
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("ftps://"))
    {
        (7, true, super::DEFAULT_PORT)
    } else {
        return None;
    };

    let without_scheme = &uri[scheme_len..];
    let (auth_host_port, remote_path) = match without_scheme.find('/') {
        Some(index) => (&without_scheme[..index], &without_scheme[index..]),
        None => (without_scheme, "/"),
    };
    let (userinfo, host_port) = match auth_host_port.rfind('@') {
        Some(index) => (&auth_host_port[..index], &auth_host_port[index + 1..]),
        None => ("", auth_host_port),
    };
    let (username, password) = if userinfo.is_empty() {
        (None, None)
    } else if let Some((username, password)) = userinfo.split_once(':') {
        (
            Some(percent_decode(username)),
            Some(percent_decode(password)),
        )
    } else {
        (Some(percent_decode(userinfo)), Some(String::new()))
    };

    let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
        let (host, suffix) = bracketed.split_once(']')?;
        let port = suffix
            .strip_prefix(':')
            .and_then(|value| value.parse().ok())
            .unwrap_or(default_port);
        (host.to_owned(), port)
    } else {
        match host_port.rsplit_once(':') {
            Some((host, port)) if port.parse::<u16>().is_ok() => {
                (host.to_owned(), port.parse().ok()?)
            }
            _ => (host_port.to_owned(), default_port),
        }
    };

    Some(FtpUri {
        host,
        port,
        username,
        password,
        remote_path: percent_decode(remote_path),
        secure,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_credentials_secure_scheme_and_encoded_path() {
        assert_eq!(
            parse_uri("FTPS://user:pass@example.test:990/a%20b"),
            Some(FtpUri {
                host: "example.test".into(),
                port: 990,
                username: Some("user".into()),
                password: Some("pass".into()),
                remote_path: "/a b".into(),
                secure: true,
            })
        );
    }

    #[test]
    fn percent_decodes_credentials_without_treating_encoded_at_as_authority() {
        let parsed = parse_uri("ftp://user%40name:pass%3Aword%40x@example.test/file")
            .expect("FTP URI should parse");

        assert_eq!(parsed.username.as_deref(), Some("user@name"));
        assert_eq!(parsed.password.as_deref(), Some("pass:word@x"));
    }

    #[test]
    fn retains_default_port_for_invalid_port_and_missing_path() {
        let parsed = parse_uri("ftp://example.test:not-a-port").unwrap();
        assert_eq!(parsed.port, 21);
        assert_eq!(parsed.remote_path, "/");
        assert!(parse_uri("http://example.test/file").is_none());
    }

    #[test]
    fn path_decoding_preserves_plus_and_malformed_escapes() {
        assert_eq!(percent_decode("a+b%20c%ZZ%3"), "a+b c%ZZ%3");
    }

    #[test]
    fn userinfo_encoding_preserves_only_unreserved_bytes() {
        assert_eq!(
            percent_encode_userinfo("user@name /é"),
            "user%40name%20%2F%C3%A9"
        );
    }

    #[test]
    fn ftp_uri_debug_redacts_credentials() {
        let uri = parse_uri("ftp://private-user:private-password@example.test/file").unwrap();

        let debug = format!("{uri:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("private-user"));
        assert!(!debug.contains("private-password"));
    }
}
