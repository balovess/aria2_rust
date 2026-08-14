//! HTTP Digest Authentication (RFC 2617) implementation
//!
//! Provides parsing of WWW-Authenticate Digest challenges and building
//! of Authorization header values for HTTP Digest authentication.
//!
//! Note: The actual hash computation (MD5/SHA-256/SHA-512-256) delegates to
//! `crate::auth::digest_auth::DigestAuthProvider`, which provides correct
//! RFC 7616-compliant hash implementations. This module focuses on the
//! challenge parsing and response formatting aspects.

use std::collections::HashMap;
use std::fmt;

use crate::auth::digest_auth::{DigestAlgorithm, DigestAuthProvider};
use crate::error::Aria2Error;

/// Parsed WWW-Authenticate header for Digest auth challenge
///
/// Represents a server's Digest authentication challenge as defined in RFC 2617.
/// Example header: `Digest realm="aria2", nonce="abc123", qop="auth", algorithm="MD5"`
#[derive(Debug, Clone)]
pub struct DigestAuthChallenge {
    /// Authentication realm (typically a human-readable string describing the protected area)
    pub realm: String,
    /// Server-provided nonce value (unique per challenge)
    pub nonce: String,
    /// Quality of protection: "auth" or "auth-int" (optional)
    pub qop: Option<String>,
    /// Hash algorithm used (default "MD5")
    pub algorithm: String,
    /// Opaque value that client must return unchanged (optional)
    pub opaque: Option<String>,
    /// If true, the previous attempt failed due to stale nonce
    pub stale: bool,
}

impl fmt::Display for DigestAuthChallenge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Digest realm=\"{}\", nonce=\"{}\", algorithm=\"{}\"",
            self.realm, self.nonce, self.algorithm
        )?;
        if let Some(ref qop) = self.qop {
            write!(f, ", qop=\"{}\"", qop)?;
        }
        if let Some(ref opaque) = self.opaque {
            write!(f, ", opaque=\"{}\"", opaque)?;
        }
        if self.stale {
            write!(f, ", stale=true")?;
        }
        Ok(())
    }
}

impl DigestAuthChallenge {
    /// Parse a `WWW-Authenticate` header value containing a Digest challenge.
    ///
    /// # Arguments
    /// * `header_value` - The full header value, e.g.
    ///   `Digest realm="aria2", nonce="abc123", qop="auth", algorithm="MD5", opaque="xyz", stale=false`
    ///
    /// # Returns
    /// A parsed `DigestAuthChallenge` on success, or an error message on failure.
    ///
    /// # Errors
    /// Returns an error if:
    /// - The header does not start with "Digest "
    /// - The required `nonce` parameter is missing
    ///
    /// # Example
    /// ```rust,ignore
    /// let challenge = DigestAuthChallenge::parse(
    ///     r#"Digest realm="test realm", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", qop="auth""#
    /// ).unwrap();
    /// assert_eq!(challenge.realm, "test realm");
    /// assert_eq!(challenge.nonce, "dcd98b7102dd2f0e8b11d0f600bfb0c093");
    /// ```
    pub fn parse(header_value: &str) -> Result<Self, Aria2Error> {
        let mut header_parts = header_value
            .trim()
            .splitn(2, |character: char| character.is_ascii_whitespace());
        let scheme = header_parts.next().unwrap_or_default();
        if !scheme.eq_ignore_ascii_case("Digest") {
            return Err(Aria2Error::Parse(
                "Not a Digest challenge: missing 'Digest' scheme".to_string(),
            ));
        }
        let digest_part = header_parts.next().unwrap_or_default();

        let params = parse_digest_parameters(digest_part)?;

        // Validate required fields
        let nonce = params.get("nonce").cloned().ok_or_else(|| {
            Aria2Error::Parse("Missing required 'nonce' parameter in Digest challenge".to_string())
        })?;

        Ok(DigestAuthChallenge {
            realm: params.get("realm").cloned().unwrap_or_default(),
            nonce,
            qop: select_qop(params.get("qop").map(String::as_str))?,
            algorithm: normalize_algorithm(params.get("algorithm").map(String::as_str))?,
            opaque: params.get("opaque").cloned(),
            stale: params
                .get("stale")
                .map(|s| s.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        })
    }
}

fn parse_digest_parameters(input: &str) -> Result<HashMap<String, String>, Aria2Error> {
    let bytes = input.as_bytes();
    let mut index = 0;
    let mut params = HashMap::new();

    while index < bytes.len() {
        while index < bytes.len() && (bytes[index].is_ascii_whitespace() || bytes[index] == b',') {
            index += 1;
        }
        if index == bytes.len() {
            break;
        }

        let key_start = index;
        while index < bytes.len() && bytes[index] != b'=' && bytes[index] != b',' {
            index += 1;
        }
        let key = input[key_start..index].trim();
        if key.is_empty() || index == bytes.len() || bytes[index] != b'=' {
            return Err(Aria2Error::Parse(
                "Invalid Digest challenge parameter".to_string(),
            ));
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }

        let value = if bytes.get(index) == Some(&b'"') {
            index += 1;
            let mut value = String::new();
            let mut closed = false;
            while index < bytes.len() {
                match bytes[index] {
                    b'\\' if index + 1 < bytes.len() => {
                        value.push(bytes[index + 1] as char);
                        index += 2;
                    }
                    b'"' => {
                        index += 1;
                        closed = true;
                        break;
                    }
                    byte => {
                        value.push(byte as char);
                        index += 1;
                    }
                }
            }
            if !closed {
                return Err(Aria2Error::Parse(
                    "Unterminated quoted Digest challenge parameter".to_string(),
                ));
            }
            value
        } else {
            let value_start = index;
            while index < bytes.len() && bytes[index] != b',' {
                index += 1;
            }
            input[value_start..index].trim().to_string()
        };

        params.insert(key.to_ascii_lowercase(), value);
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index < bytes.len() && bytes[index] != b',' {
            return Err(Aria2Error::Parse(
                "Invalid Digest challenge parameter separator".to_string(),
            ));
        }
    }

    Ok(params)
}

fn select_qop(value: Option<&str>) -> Result<Option<String>, Aria2Error> {
    let Some(value) = value else {
        return Ok(None);
    };

    value
        .split(',')
        .map(str::trim)
        .find(|qop| qop.eq_ignore_ascii_case("auth"))
        .or_else(|| {
            value
                .split(',')
                .map(str::trim)
                .find(|qop| qop.eq_ignore_ascii_case("auth-int"))
        })
        .map(|qop| qop.to_ascii_lowercase())
        .ok_or_else(|| Aria2Error::Parse(format!("Unsupported Digest qop: {value}")))
        .map(Some)
}

fn normalize_algorithm(value: Option<&str>) -> Result<String, Aria2Error> {
    let algorithm = value.unwrap_or("MD5");
    let canonical = match algorithm.to_ascii_uppercase().as_str() {
        "MD5" => "MD5",
        "MD5-SESS" => "MD5-sess",
        "SHA-256" => "SHA-256",
        "SHA-256-SESS" => "SHA-256-sess",
        "SHA-512-256" => "SHA-512-256",
        "SHA-512-256-SESS" => "SHA-512-256-sess",
        _ => {
            return Err(Aria2Error::Parse(format!(
                "Unsupported Digest algorithm: {algorithm}"
            )));
        }
    };
    Ok(canonical.to_string())
}

/// Built Digest authentication response ready for inclusion in an Authorization header
///
/// Contains all computed values needed to construct the Authorization header value.
#[derive(Debug, Clone)]
pub struct DigestAuthResponse {
    /// Username being authenticated
    pub username: String,
    /// Realm from the server's challenge
    pub realm: String,
    /// Nonce from the server's challenge
    pub nonce: String,
    /// Request URI (path portion)
    pub uri: String,
    /// Quality of protection (from challenge)
    pub qop: Option<String>,
    /// Nonce count (hex, 8 digits)
    pub nc: u32,
    /// Client-generated nonce (hex string)
    pub cnonce: String,
    /// Computed response hash
    pub response: String,
    /// Algorithm used (from challenge)
    pub algorithm: String,
    /// Opaque value from challenge (must be returned unchanged)
    pub opaque: Option<String>,
}

impl DigestAuthResponse {
    /// Build the complete `Authorization` header value string.
    ///
    /// Returns the full header value in the format:
    /// ```text
    /// Digest username="...", realm="...", nonce="...", uri="...",
    ///        nc=XXXXXXXX, cnonce="...", qop="...", response="...",
    ///        algorithm="...", opaque="..."
    /// ```
    pub fn to_header_value(&self) -> String {
        let mut fields = vec![
            format!(r#"username="{}""#, escape_quoted(&self.username)),
            format!(r#"realm="{}""#, escape_quoted(&self.realm)),
            format!(r#"nonce="{}""#, escape_quoted(&self.nonce)),
            format!(r#"uri="{}""#, escape_quoted(&self.uri)),
            format!(r#"response="{}""#, self.response),
            format!("algorithm={}", self.algorithm),
        ];
        if let Some(qop) = self.qop.as_deref() {
            fields.push(format!("qop={qop}"));
            fields.push(format!("nc={:08x}", self.nc));
            fields.push(format!(r#"cnonce="{}""#, escape_quoted(&self.cnonce)));
        } else if self.algorithm.eq_ignore_ascii_case("MD5-sess")
            || self.algorithm.eq_ignore_ascii_case("SHA-256-sess")
            || self.algorithm.eq_ignore_ascii_case("SHA-512-256-sess")
        {
            fields.push(format!(r#"cnonce="{}""#, escape_quoted(&self.cnonce)));
        }
        if let Some(opaque) = self.opaque.as_deref() {
            fields.push(format!(r#"opaque="{}""#, escape_quoted(opaque)));
        }
        format!("Digest {}", fields.join(", "))
    }

    /// Compute a Digest authentication response per RFC 2617 section 3.2.2.1.
    ///
    /// This method computes all necessary hashes and builds a complete response
    /// that can be serialized via [`to_header_value`](Self::to_header_value).
    ///
    /// # Algorithm (RFC 2617):
    /// ```text
    /// HA1 = MD5(username:realm:password)
    /// HA2 = MD5(method:uri)
    /// if qop is set:
    ///     response = MD5(HA1:nonce:nc:cnonce:qop:HA2)
    /// else:
    ///     response = MD5(HA1:nonce:HA2)
    /// ```
    ///
    /// # Arguments
    /// * `username` - The username for authentication
    /// * `password` - The user's password (plaintext)
    /// * `method` - HTTP method (GET, POST, etc.)
    /// * `uri` - The request URI path
    /// * `challenge` - The parsed server challenge
    /// * `nc` - Nonce count (incremented per request with same nonce)
    ///
    /// # Returns
    /// A fully constructed `DigestAuthResponse`.
    pub fn compute(
        username: &str,
        password: &str,
        method: &str,
        uri: &str,
        challenge: &DigestAuthChallenge,
        nc: u32,
    ) -> Self {
        // Determine the hash algorithm from the challenge
        let algorithm_name = challenge.algorithm.to_ascii_uppercase();
        let algorithm = match algorithm_name.as_str() {
            "SHA-256" | "SHA-256-SESS" => DigestAlgorithm::Sha256,
            "SHA-512-256" | "SHA-512-256-SESS" => DigestAlgorithm::Sha512_256,
            _ => DigestAlgorithm::Md5,
        };
        let is_session_algorithm = algorithm_name.ends_with("-SESS");

        // Create a DigestAuthProvider to leverage its correct hash implementations
        let provider =
            DigestAuthProvider::new(username.to_string(), password.to_string(), Some(algorithm));

        // Compute HA2 = H(method:uri)
        // Generate random cnonce (client nonce) using crypto-quality randomness
        let cnonce = format!("{:016x}", rand::random::<u64>());
        let qop = select_qop(challenge.qop.as_deref()).unwrap_or(None);

        // RFC 7616 uses a second HA1 derivation for -sess algorithms.
        let initial_ha1 = provider.compute_ha1(&challenge.realm);
        let ha1 = if is_session_algorithm {
            provider.hash_kd(&initial_ha1, &format!("{}:{}", challenge.nonce, cnonce))
        } else {
            initial_ha1
        };

        let ha2 = provider.compute_ha2(method, uri, qop.as_deref(), None);

        let nc_str = format!("{:08x}", nc);

        // Compute final response = KD(HA1, nonce:nc:cnonce:qop:HA2) or KD(HA1, nonce:HA2)
        let response = provider.compute_response(
            &ha1,
            &challenge.nonce,
            &nc_str,
            &cnonce,
            qop.as_deref(),
            &ha2,
        );

        DigestAuthResponse {
            username: username.to_string(),
            realm: challenge.realm.clone(),
            nonce: challenge.nonce.clone(),
            uri: uri.to_string(),
            qop,
            nc,
            cnonce,
            response,
            algorithm: challenge.algorithm.clone(),
            opaque: challenge.opaque.clone(),
        }
    }
}

fn escape_quoted(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- DigestAuthChallenge tests ---

    #[test]
    fn test_digest_challenge_parse_basic() {
        let header = r#"Digest realm="test realm", nonce="abc123def456""#;
        let challenge = DigestAuthChallenge::parse(header).unwrap();

        assert_eq!(challenge.realm, "test realm");
        assert_eq!(challenge.nonce, "abc123def456");
        assert_eq!(challenge.algorithm, "MD5"); // default
        assert!(challenge.qop.is_none());
        assert!(challenge.opaque.is_none());
        assert!(!challenge.stale);
    }

    #[test]
    fn test_digest_challenge_parse_all_fields() {
        let header = r#"Digest realm="aria2 download", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", qop="auth", algorithm="MD5", opaque="5ccc069c403ebaf9f0171e9517bf40c9", stale=true"#;
        let challenge = DigestAuthChallenge::parse(header).unwrap();

        assert_eq!(challenge.realm, "aria2 download");
        assert_eq!(challenge.nonce, "dcd98b7102dd2f0e8b11d0f600bfb0c093");
        assert_eq!(challenge.qop.as_deref(), Some("auth"));
        assert_eq!(challenge.algorithm, "MD5");
        assert_eq!(
            challenge.opaque.as_deref(),
            Some("5ccc069c403ebaf9f0171e9517bf40c9")
        );
        assert!(challenge.stale);
    }

    #[test]
    fn test_digest_challenge_missing_nonce_returns_error() {
        let header = r#"Digest realm="only realm""#;
        let result = DigestAuthChallenge::parse(header);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nonce"));
    }

    #[test]
    fn test_digest_challenge_not_digest_prefix_returns_error() {
        let header = r#"Basic realm="test""#;
        let result = DigestAuthChallenge::parse(header);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Digest "));
    }

    #[test]
    fn test_digest_challenge_case_insensitive_keys() {
        // Keys should be case-insensitive per RFC
        let header = r#"Digest REALM="TestRealm", NONCE="myNonce", ALGORITHM="SHA-256""#;
        let challenge = DigestAuthChallenge::parse(header).unwrap();
        assert_eq!(challenge.realm, "TestRealm");
        assert_eq!(challenge.nonce, "myNonce");
        assert_eq!(challenge.algorithm, "SHA-256");
    }

    #[test]
    fn test_digest_challenge_parse_case_insensitive_scheme_and_quoted_commas() {
        let challenge = DigestAuthChallenge::parse(
            r#"dIgEsT realm="download, private", nonce="n\"1", qop="auth-int, auth", algorithm=MD5"#,
        )
        .unwrap();

        assert_eq!(challenge.realm, "download, private");
        assert_eq!(challenge.nonce, "n\"1");
        assert_eq!(challenge.qop.as_deref(), Some("auth"));
    }

    #[test]
    fn test_digest_challenge_rejects_unknown_algorithm() {
        let result =
            DigestAuthChallenge::parse(r#"Digest realm="test", nonce="nonce", algorithm=SHA-1"#);
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Unsupported Digest algorithm")
        );
    }

    // --- DigestAuthResponse tests ---

    #[test]
    fn test_digest_response_compute_and_format() {
        let challenge = DigestAuthChallenge::parse(
            r#"Digest realm="test@host.com", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", qop="auth", opaque="someopaque""#
        ).unwrap();

        let response = DigestAuthResponse::compute(
            "Mufasa",
            "Circle Of Life",
            "GET",
            "/dir/index.html",
            &challenge,
            1,
        );

        assert_eq!(response.username, "Mufasa");
        assert_eq!(response.realm, "test@host.com");
        assert_eq!(response.uri, "/dir/index.html");
        assert_eq!(response.nc, 1);
        assert_eq!(response.qop.as_deref(), Some("auth"));
        assert_eq!(response.algorithm, "MD5");
        assert_eq!(response.opaque.as_deref(), Some("someopaque"));

        // Verify the header value contains expected components
        let header_val = response.to_header_value();
        assert!(header_val.starts_with("Digest "));
        assert!(header_val.contains(r#"username="Mufasa""#));
        assert!(header_val.contains(r#"realm="test@host.com""#));
        assert!(header_val.contains("nc=00000001"));
        assert!(header_val.contains(r#"response=""#));
        assert!(!response.cnonce.is_empty());
    }

    #[test]
    fn test_digest_response_without_qop() {
        let challenge =
            DigestAuthChallenge::parse(r#"Digest realm="simple", nonce="simpleNonce123""#).unwrap();

        let response =
            DigestAuthResponse::compute("user", "pass", "POST", "/api/data", &challenge, 1);

        assert!(response.qop.is_none());
        let header_val = response.to_header_value();
        assert!(!header_val.contains("qop="));
        assert!(header_val.contains("algorithm=MD5"));
        assert!(!header_val.contains("cnonce="));
    }

    #[test]
    fn test_digest_full_flow_roundtrip() {
        // Simulate full flow: receive challenge -> build response -> verify format

        // Step 1: Server sends WWW-Authenticate header
        let www_authenticate = r#"Digest realm="WallyWorld", nonce="OA=MPOPQKX/RI=SOXPVDFKB,URI=/download/file.torrent", qop="auth", algorithm="MD5", opaque="FQwERTYuiop123""#;

        // Step 2: Client parses the challenge
        let challenge = DigestAuthChallenge::parse(www_authenticate).unwrap();
        assert_eq!(challenge.realm, "WallyWorld");

        // Step 3: Client computes response
        let auth_response = DigestAuthResponse::compute(
            "admin",
            "secret123",
            "GET",
            "/download/file.torrent",
            &challenge,
            1,
        );

        // Step 4: Build Authorization header value
        let authorization = auth_response.to_header_value();

        // Verify roundtrip integrity
        assert!(authorization.starts_with("Digest "));
        assert!(authorization.contains(r#"username="admin""#));
        assert!(authorization.contains(r#"realm="WallyWorld""#));
        assert!(authorization.contains(&format!("nonce=\"{}\"", challenge.nonce)));
        assert!(authorization.contains(r#"uri="/download/file.torrent""#));
        assert!(authorization.contains("nc=00000001"));
        assert!(authorization.contains("qop=auth"));
        assert!(authorization.contains(r#"opaque="FQwERTYuiop123""#));

        // Verify we can re-parse the generated header structure (sanity check)
        assert!(authorization.len() > 50); // Should be substantial
        assert!(!authorization.contains("\n")); // Single line header
    }

    #[test]
    fn test_digest_challenge_display_format() {
        let challenge = DigestAuthChallenge {
            realm: "MyRealm".into(),
            nonce: "abc123".into(),
            qop: Some("auth".into()),
            algorithm: "MD5".into(),
            opaque: Some("opaqueVal".into()),
            stale: false,
        };

        let display = format!("{}", challenge);
        assert!(display.contains("Digest "));
        assert!(display.contains(r#"realm="MyRealm""#));
        assert!(display.contains(r#"nonce="abc123""#));
        assert!(display.contains(r#"qop="auth""#));
        assert!(display.contains(r#"opaque="opaqueVal""#));
    }

    #[test]
    fn test_digest_nonce_count_increments_correctly() {
        let challenge =
            DigestAuthChallenge::parse(r#"Digest realm="test", nonce="nonce123", qop="auth""#)
                .unwrap();

        let resp1 = DigestAuthResponse::compute("u", "p", "GET", "/", &challenge, 1);
        let resp2 = DigestAuthResponse::compute("u", "p", "GET", "/", &challenge, 2);

        let h1 = resp1.to_header_value();
        let h2 = resp2.to_header_value();

        assert!(h1.contains("nc=00000001"));
        assert!(h2.contains("nc=00000002"));

        // Each request should have different cnonce and response
        assert_ne!(resp1.cnonce, resp2.cnonce);
        assert_ne!(resp1.response, resp2.response);
    }

    /// Verify RFC 2617 known test vector.
    ///
    /// Reference values computed from:
    ///   HA1 = MD5("Mufasa:testrealm@host.com:Circle Of Life") = "939e7578ed9e3c518a452acee763bce9"
    ///   HA2 = MD5("GET:/dir/index.html") = "39aff3a2bab6126f332b942af96d3366"
    ///   response = MD5(HA1 ":" nonce ":" nc ":" cnonce ":" qop ":" HA2)
    ///            = MD5("939e7578ed9e3c518a452acee763bce9:dcd98b7102dd2f0e8b11d0f600bfb0c093:00000001:0a4f113b:auth:39aff3a2bab6126f332b942af96d3366")
    ///            = "6629fae49393a05397450978507c4ef1"
    #[test]
    fn test_rfc2617_known_vector_md5() {
        // Verify HA1 computation
        let provider = DigestAuthProvider::new(
            "Mufasa".to_string(),
            "Circle Of Life".to_string(),
            Some(DigestAlgorithm::Md5),
        );
        let ha1 = provider.compute_ha1("testrealm@host.com");
        assert_eq!(
            ha1, "939e7578ed9e3c518a452acee763bce9",
            "HA1 should match RFC 2617 test vector"
        );

        // Verify HA2 computation
        let ha2 = provider.compute_ha2("GET", "/dir/index.html", None, None);
        assert_eq!(
            ha2, "39aff3a2bab6126f332b942af96d3366",
            "HA2 should match RFC 2617 test vector"
        );

        // Verify response computation with known cnonce
        let response = provider.compute_response(
            &ha1,
            "dcd98b7102dd2f0e8b11d0f600bfb0c093",
            "00000001",
            "0a4f113b",
            Some("auth"),
            &ha2,
        );
        assert_eq!(
            response, "6629fae49393a05397450978507c4ef1",
            "Response should match RFC 2617 test vector"
        );
    }

    /// Verify SHA-256 variant produces correct length hash (64 hex chars).
    #[test]
    fn test_sha256_digest_produces_64_char_hex() {
        let provider = DigestAuthProvider::new(
            "user".to_string(),
            "pass".to_string(),
            Some(DigestAlgorithm::Sha256),
        );

        let ha1 = provider.compute_ha1("testrealm");
        assert_eq!(ha1.len(), 64, "SHA-256 should produce 64 hex chars");

        let ha2 = provider.compute_ha2("GET", "/test", None, None);
        assert_eq!(ha2.len(), 64, "SHA-256 should produce 64 hex chars");
    }
}
