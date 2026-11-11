use base64::{Engine as _, engine::general_purpose::STANDARD};

/// Build the HTTP Basic authentication field value for a credential pair.
pub fn basic_authorization(username: &str, password: &str) -> String {
    let credentials = format!("{username}:{password}");
    format!("Basic {}", STANDARD.encode(credentials))
}

/// Build the HTTP Bearer authentication field value.
pub fn bearer_authorization(token: &str) -> String {
    format!("Bearer {token}")
}

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
        basic_authorization(&self.username, &self.password)
    }
}

pub struct HttpAuth;

impl HttpAuth {
    pub fn apply_auth(
        request: crate::http::request::HttpRequest,
        credentials: &AuthCredentials,
    ) -> crate::http::request::HttpRequest {
        match credentials.scheme {
            AuthScheme::Basic => {
                request.with_header("Authorization", &credentials.basic_auth_header())
            }
            AuthScheme::Digest => request,
        }
    }

    pub fn parse_www_authenticate(header_value: &str) -> Option<AuthChallenge> {
        let header = header_value.trim();
        let scheme = header.split_ascii_whitespace().next()?;
        if scheme.eq_ignore_ascii_case("basic") {
            return Some(AuthChallenge::Basic);
        }
        if !scheme.eq_ignore_ascii_case("digest") {
            return None;
        }
        let challenge = crate::http::digest::DigestChallenge::parse(header).ok()?;
        Some(AuthChallenge::Digest(DigestChallenge {
            realm: challenge.realm,
            nonce: challenge.nonce,
            qop: challenge.qop.unwrap_or_default(),
            algorithm: challenge.algorithm,
            opaque: challenge.opaque.unwrap_or_default(),
            stale: challenge.stale,
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
        let protocol_challenge = crate::http::digest::DigestChallenge {
            realm: challenge.realm.clone(),
            nonce: challenge.nonce.clone(),
            qop: (!challenge.qop.is_empty()).then(|| challenge.qop.clone()),
            algorithm: challenge.algorithm.clone(),
            opaque: (!challenge.opaque.is_empty()).then(|| challenge.opaque.clone()),
            stale: challenge.stale,
        };
        crate::http::digest::DigestResponse::compute_with_cnonce(
            &credentials.username,
            &credentials.password,
            method,
            uri,
            &protocol_challenge,
            nc,
            cnonce,
        )
        .to_header_value()
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
        assert_eq!(bearer_authorization("token"), "Bearer token");
    }

    #[test]
    fn test_parse_basic_challenge() {
        let result = HttpAuth::parse_www_authenticate("Basic");
        assert!(matches!(result, Some(AuthChallenge::Basic)));
        assert!(matches!(
            HttpAuth::parse_www_authenticate("bAsIc realm=\"proxy\""),
            Some(AuthChallenge::Basic)
        ));
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
    fn parses_digest_challenges_with_quoted_commas_and_supported_algorithms() {
        let result = HttpAuth::parse_www_authenticate(
            r#"dIgEsT realm="private, proxy", nonce="n1", qop="auth-int", algorithm=SHA-256-sess"#,
        );
        let Some(AuthChallenge::Digest(challenge)) = result else {
            panic!("expected parsed Digest challenge");
        };
        assert_eq!(challenge.realm, "private, proxy");
        assert_eq!(challenge.qop, "auth-int");
        assert_eq!(challenge.algorithm, "SHA-256-sess");
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
        let response =
            HttpAuth::build_digest_response(&challenge, &creds, "GET", "/path", 1, "abc");
        assert!(response.starts_with("Digest "));
        assert!(response.contains("username=\"user\""));
        assert!(response.contains("realm=\"testrealm\""));
    }

    #[test]
    fn computes_digest_with_explicit_cnonce_through_shared_protocol_engine() {
        let challenge = DigestChallenge {
            realm: "private, proxy".to_owned(),
            nonce: "n1".to_owned(),
            qop: "auth-int".to_owned(),
            algorithm: "SHA-256-sess".to_owned(),
            opaque: "opaque".to_owned(),
            stale: false,
        };
        let credentials = AuthCredentials::new_digest("user", "pass");
        let response = HttpAuth::build_digest_response(
            &challenge,
            &credentials,
            "CONNECT",
            "ftp.example.test:21",
            1,
            "fixed-cnonce",
        );
        assert!(response.contains("realm=\"private, proxy\""));
        assert!(response.contains("qop=auth-int"));
        assert!(response.contains("algorithm=SHA-256-sess"));
        assert!(response.contains("cnonce=\"fixed-cnonce\""));
        assert!(response.contains("opaque=\"opaque\""));
        let digest = response
            .split("response=\"")
            .nth(1)
            .and_then(|tail| tail.split('"').next())
            .expect("response hash is emitted");
        assert_eq!(digest.len(), 64);
    }
}
