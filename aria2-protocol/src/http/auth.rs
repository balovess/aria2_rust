use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum AuthScheme {
    Basic,
    Digest,
}

#[derive(Debug, Clone)]
pub struct AuthCredentials {
    pub username: String,
    pub password: String,
    pub scheme: AuthScheme,
}

impl AuthCredentials {
    pub fn new_basic(username: &str, password: &str) -> Self {
        Self {
            username: username.to_string(),
            password: password.to_string(),
            scheme: AuthScheme::Basic,
        }
    }

    pub fn new_digest(username: &str, password: &str) -> Self {
        Self {
            username: username.to_string(),
            password: password.to_string(),
            scheme: AuthScheme::Digest,
        }
    }

    pub fn basic_auth_header(&self) -> String {
        let credentials = format!("{}:{}", self.username, self.password);
        let encoded = STANDARD.encode(credentials);
        format!("Basic {}", encoded)
    }
}

pub struct HttpAuth;

impl HttpAuth {
    pub fn apply_auth(
        request: crate::http::request::HttpRequest,
        credentials: &AuthCredentials,
    ) -> crate::http::request::HttpRequest {
        match credentials.scheme {
            AuthScheme::Basic => request.with_header("Authorization", &credentials.basic_auth_header()),
            AuthScheme::Digest => request,
        }
    }

    pub fn parse_www_authenticate(header_value: &str) -> Option<AuthChallenge> {
        let header = header_value.trim();
        if header.eq_ignore_ascii_case("basic") || header.starts_with("Basic ") {
            return Some(AuthChallenge::Basic);
        }

        if !header.starts_with("Digest ") {
            return None;
        }

        let params_str = &header[7..];
        let mut params = HashMap::new();
        for part in params_str.split(',') {
            let part = part.trim();
            if let Some((key, value)) = part.split_once('=') {
                let key = key.trim().to_lowercase();
                let mut value = value.trim();
                if value.starts_with('"') && value.ends_with('"') {
                    value = &value[1..value.len() - 1];
                }
                params.insert(key, value.to_string());
            }

            if let Some((key, _)) = part.split_once('=') {
                let key = key.trim().to_lowercase();
                if !params.contains_key(&key) {
                    params.insert(key, String::new());
                }
            }
        }

        Some(AuthChallenge::Digest(DigestChallenge {
            realm: params.get("realm").cloned().unwrap_or_default(),
            nonce: params.get("nonce").cloned().unwrap_or_default(),
            qop: params.get("qop").cloned().unwrap_or_default(),
            algorithm: params.get("algorithm").cloned().unwrap_or_else(|| "MD5".to_string()),
            opaque: params.get("opaque").cloned().unwrap_or_default(),
            stale: params.get("stale")
                .map(|v: &String| v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        }))
    }

    pub fn build_digest_response(
        challenge: &DigestChallenge,
        credentials: &AuthCredentials,
        method: &str,
        uri: &str,
        nc: u32,
        cnonce: &str,
    ) -> String {
        let ha1 = Self::compute_ha1(
            &credentials.username,
            &challenge.realm,
            &credentials.password,
            &challenge.algorithm,
        );

        let ha2 = Self::compute_ha2(method, uri);

        let response = if challenge.qop.is_empty() {
            format!("{:x}", md5::compute(format!("{}:{}{}", ha1, challenge.nonce, ha2)))
        } else {
            format!(
                "{:x}",
                md5::compute(format!(
                    "{}:{}:{:08x}:{}:{}:{}",
                    ha1, challenge.nonce, nc, cnonce, challenge.qop, ha2
                ))
            )
        };

        let mut parts = vec![
            format!("username=\"{}\"", credentials.username),
            format!("realm=\"{}\"", challenge.realm),
            format!("nonce=\"{}\"", challenge.nonce),
            format!("uri=\"{}\"", uri),
            format!("response=\"{}\"", response),
            format!("algorithm={}", challenge.algorithm),
        ];

        if !challenge.qop.is_empty() {
            parts.push(format!("qop={}", challenge.qop));
            parts.push(format!("nc={:08x}", nc));
            parts.push(format!("cnonce=\"{}\"", cnonce));
        }

        if !challenge.opaque.is_empty() {
            parts.push(format!("opaque=\"{}\"", challenge.opaque));
        }

        format!("Digest {}", parts.join(", "))
    }

    fn compute_ha1(username: &str, realm: &str, password: &str, _algorithm: &str) -> String {
        let a1 = format!("{}:{}:{}", username, realm, password);
        format!("{:x}", md5::compute(a1))
    }

    fn compute_ha2(method: &str, uri: &str) -> String {
        format!("{:x}", md5::compute(format!("{}:{}", method, uri)))
    }
}

#[derive(Debug, Clone)]
pub enum AuthChallenge {
    Basic,
    Digest(DigestChallenge),
}

#[derive(Debug, Clone)]
pub struct DigestChallenge {
    pub realm: String,
    pub nonce: String,
    pub qop: String,
    pub algorithm: String,
    pub opaque: String,
    pub stale: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_auth_header() {
        let creds = AuthCredentials::new_basic("admin", "secret123");
        let header = creds.basic_auth_header();
        assert!(header.starts_with("Basic "));
        assert!(!header.contains("admin"));
        assert!(!header.contains("secret123"));
    }

    #[test]
    fn test_parse_basic_challenge() {
        let result = HttpAuth::parse_www_authenticate("Basic");
        assert!(matches!(result, Some(AuthChallenge::Basic)));
    }

    #[test]
    fn test_parse_digest_challenge() {
        let header = r#"Digest realm="testrealm@host.com", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", qop="auth", algorithm=MD5"#;
        let result = HttpAuth::parse_www_authenticate(header);
        assert!(matches!(result, Some(AuthChallenge::Digest(_))));
        if let Some(AuthChallenge::Digest(d)) = result {
            assert_eq!(d.realm, "testrealm@host.com");
            assert_eq!(d.nonce, "dcd98b7102dd2f0e8b11d0f600bfb0c093");
            assert_eq!(d.qop, "auth");
        }
    }

    #[test]
    fn test_build_digest_response() {
        let challenge = DigestChallenge {
            realm: "testrealm".to_string(),
            nonce: "abcdef123456".to_string(),
            qop: "auth".to_string(),
            algorithm: "MD5".to_string(),
            opaque: "".to_string(),
            stale: false,
        };
        let creds = AuthCredentials::new_digest("user", "pass");
        let response = HttpAuth::build_digest_response(&challenge, &creds, "GET", "/path", 1, "abc");
        assert!(response.starts_with("Digest "));
        assert!(response.contains("username=\"user\""));
        assert!(response.contains("realm=\"testrealm\""));
    }
}
